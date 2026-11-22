#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `webhook::send` against a hand-written HTTP responder on 127.0.0.1.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use aaf_domain::event::EventKind;
use aaf_engine::webhook::{WebhookError, WebhookEvent, WebhookOptions, mask_url, send};

struct Captured {
    request_line: String,
    /// Header names lower-cased.
    headers: Vec<(String, String)>,
    body: String,
}

impl Captured {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Serves exactly `statuses.len()` connections, one scripted status each,
/// then stops listening. Returns the port and the captured requests.
fn serve(statuses: &[u16]) -> (u16, mpsc::Receiver<Captured>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    let statuses = statuses.to_vec();
    thread::spawn(move || {
        for status in statuses {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            let mut headers = Vec::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let line = line.trim_end();
                if line.is_empty() {
                    break;
                }
                let (name, value) = line.split_once(':').unwrap();
                headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
            }
            let length: usize = headers
                .iter()
                .find(|(k, _)| k == "content-length")
                .map(|(_, v)| v.parse().unwrap())
                .unwrap();
            let mut body = vec![0_u8; length];
            reader.read_exact(&mut body).unwrap();
            // The body is deliberately not JSON: the client must not parse it.
            let reply = format!(
                "HTTP/1.1 {status} X\r\nContent-Length: 9\r\nConnection: close\r\n\r\n<<junk>>!"
            );
            reader.get_mut().write_all(reply.as_bytes()).unwrap();
            tx.send(Captured {
                request_line: request_line.trim_end().to_string(),
                headers,
                body: String::from_utf8(body).unwrap(),
            })
            .unwrap();
        }
    });
    (port, rx)
}

fn event() -> WebhookEvent {
    WebhookEvent {
        event_id: "evt-42".to_string(),
        kind: EventKind::GateResult,
        task_id: Some("task-7".to_string()),
        status: "BLOCKED".to_string(),
        summary: "gate failed: 2 findings".to_string(),
        created_at: 1_800_000_000,
    }
}

/// Options whose sleeper records instead of waiting.
fn options() -> (WebhookOptions, Arc<Mutex<Vec<Duration>>>) {
    let slept = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&slept);
    let opts = WebhookOptions {
        connect_timeout: Duration::from_secs(2),
        timeout: Duration::from_secs(5),
        sleeper: Box::new(move |d| sink.lock().unwrap().push(d)),
        ..WebhookOptions::default()
    };
    (opts, slept)
}

#[test]
fn defaults_follow_the_network_rules() {
    let opts = WebhookOptions::default();
    assert_eq!(opts.connect_timeout, Duration::from_secs(10));
    assert_eq!(opts.timeout, Duration::from_secs(15));
    assert_eq!(opts.max_retries, 2);
}

#[test]
fn empty_url_is_a_noop() {
    let (opts, slept) = options();
    assert_eq!(send("", &event(), &opts), Ok(()));
    assert_eq!(send("   ", &event(), &opts), Ok(()));
    assert!(slept.lock().unwrap().is_empty());
}

#[test]
fn success_posts_one_json_document_with_idempotency_key() {
    let (port, rx) = serve(&[200]);
    let (opts, slept) = options();
    let url = format!("http://127.0.0.1:{port}/hook?token=s3cret");
    assert_eq!(send(&url, &event(), &opts), Ok(()));

    let got = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(got.request_line, "POST /hook?token=s3cret HTTP/1.1");
    assert_eq!(got.header("idempotency-key"), Some("evt-42"));
    assert_eq!(got.header("content-type"), Some("application/json"));
    let body: serde_json::Value = serde_json::from_str(&got.body).unwrap();
    assert_eq!(
        body,
        serde_json::json!({
            "event_id": "evt-42",
            "kind": "gate_result",
            "task_id": "task-7",
            "status": "BLOCKED",
            "summary": "gate failed: 2 findings",
            "created_at": 1_800_000_000_i64,
        })
    );
    assert!(slept.lock().unwrap().is_empty());
    // Exactly one request: the server thread has stopped accepting.
    assert!(rx.recv_timeout(Duration::from_secs(5)).is_err());
}

#[test]
fn retries_on_500_then_succeeds_with_the_same_key_and_body() {
    let (port, rx) = serve(&[500, 204]);
    let (opts, slept) = options();
    let url = format!("http://127.0.0.1:{port}/hook");
    assert_eq!(send(&url, &event(), &opts), Ok(()));

    let first = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let second = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(first.header("idempotency-key"), Some("evt-42"));
    assert_eq!(second.header("idempotency-key"), Some("evt-42"));
    assert_eq!(first.body, second.body);
    assert_eq!(slept.lock().unwrap().len(), 1);
}

#[test]
fn retries_on_429() {
    let (port, rx) = serve(&[429, 200]);
    let (opts, slept) = options();
    let url = format!("http://127.0.0.1:{port}/hook");
    assert_eq!(send(&url, &event(), &opts), Ok(()));
    assert_eq!(rx.iter().count(), 2);
    assert_eq!(slept.lock().unwrap().len(), 1);
}

#[test]
fn does_not_retry_on_400() {
    // A second scripted reply would be a 200: reaching it would mean a retry.
    let (port, rx) = serve(&[400, 200]);
    let (opts, slept) = options();
    let url = format!("http://127.0.0.1:{port}/hook");
    let err = send(&url, &event(), &opts).unwrap_err();
    assert!(
        matches!(
            err,
            WebhookError::Status {
                status: 400,
                attempts: 1,
                ..
            }
        ),
        "{err:?}"
    );
    assert!(slept.lock().unwrap().is_empty());
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
}

#[test]
fn does_not_follow_or_retry_a_redirect() {
    let (port, rx) = serve(&[302, 200]);
    let (opts, slept) = options();
    let url = format!("http://127.0.0.1:{port}/hook");
    let err = send(&url, &event(), &opts).unwrap_err();
    assert!(
        matches!(
            err,
            WebhookError::Status {
                status: 302,
                attempts: 1,
                ..
            }
        ),
        "{err:?}"
    );
    assert!(slept.lock().unwrap().is_empty());
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
}

#[test]
fn gives_up_after_two_retries_with_growing_jittered_backoff() {
    let (port, rx) = serve(&[500, 503, 502]);
    let (mut opts, slept) = options();
    opts.base_backoff = Duration::from_millis(100);
    let url = format!("http://127.0.0.1:{port}/hook");
    let err = send(&url, &event(), &opts).unwrap_err();
    assert!(
        matches!(
            err,
            WebhookError::Status {
                status: 502,
                attempts: 3,
                ..
            }
        ),
        "{err:?}"
    );
    assert_eq!(rx.iter().count(), 3);

    let slept = slept.lock().unwrap();
    assert_eq!(slept.len(), 2);
    // base * 2^n plus at most half of that again as jitter.
    assert!(
        slept[0] >= Duration::from_millis(100) && slept[0] <= Duration::from_millis(150),
        "{slept:?}"
    );
    assert!(
        slept[1] >= Duration::from_millis(200) && slept[1] <= Duration::from_millis(300),
        "{slept:?}"
    );
}

#[test]
fn retry_limit_is_capped_at_two_even_when_asked_for_more() {
    let (port, rx) = serve(&[500, 500, 500]);
    let (mut opts, slept) = options();
    opts.max_retries = 9;
    let url = format!("http://127.0.0.1:{port}/hook");
    let err = send(&url, &event(), &opts).unwrap_err();
    assert!(
        matches!(err, WebhookError::Status { attempts: 3, .. }),
        "{err:?}"
    );
    assert_eq!(rx.iter().count(), 3);
    assert_eq!(slept.lock().unwrap().len(), 2);
}

#[test]
fn connection_errors_are_retried_and_reported_with_a_masked_url() {
    // Bind then drop: nothing listens on this port any more.
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let (opts, slept) = options();
    let url =
        format!("http://alice:hunter2@127.0.0.1:{port}/services/T0/B0/pathsecret?token=s3cret");
    let err = send(&url, &event(), &opts).unwrap_err();
    assert!(
        matches!(err, WebhookError::Transport { attempts: 3, .. }),
        "{err:?}"
    );
    assert_eq!(slept.lock().unwrap().len(), 2);

    for text in [err.to_string(), format!("{err:?}")] {
        for secret in ["alice", "hunter2", "s3cret", "pathsecret", "token"] {
            assert!(!text.contains(secret), "{secret} leaked in: {text}");
        }
        assert!(text.contains(&format!("127.0.0.1:{port}")), "{text}");
    }
}

#[test]
fn status_errors_mask_the_url() {
    let (port, _rx) = serve(&[403]);
    let (opts, _) = options();
    let url = format!("http://127.0.0.1:{port}/hook/pathsecret?token=s3cret");
    let text = send(&url, &event(), &opts).unwrap_err().to_string();
    assert!(text.contains("403"), "{text}");
    assert!(!text.contains("s3cret"), "{text}");
    assert!(!text.contains("pathsecret"), "{text}");
    assert!(!text.contains("junk"), "response body leaked: {text}");
}

#[test]
fn rejects_non_http_schemes_without_sending() {
    let (opts, slept) = options();
    for url in [
        "ftp://bob:pw123@example.invalid/x?key=s3cret",
        "file:///etc/passwd",
        "javascript:alert(1)",
        "example.invalid/hook?key=s3cret",
        "http://",
        "http:///nohost",
    ] {
        let err = send(url, &event(), &opts).unwrap_err();
        assert!(
            matches!(err, WebhookError::InvalidUrl { .. }),
            "{url}: {err:?}"
        );
        let text = format!("{err} {err:?}");
        for secret in ["pw123", "s3cret", "bob"] {
            assert!(!text.contains(secret), "{secret} leaked in: {text}");
        }
    }
    assert!(slept.lock().unwrap().is_empty());
}

#[test]
fn https_scheme_is_accepted_by_validation() {
    // Nothing speaks TLS on this port, so the attempt fails as transport,
    // which proves the URL got past the scheme check.
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let (mut opts, _) = options();
    opts.max_retries = 0;
    let err = send(&format!("https://127.0.0.1:{port}/x"), &event(), &opts).unwrap_err();
    assert!(
        matches!(err, WebhookError::Transport { attempts: 1, .. }),
        "{err:?}"
    );
}

#[test]
fn rejects_an_event_id_that_cannot_be_a_header_value() {
    let (opts, _) = options();
    for bad in ["", "a\r\nX-Injected: 1", "caf\u{e9}"] {
        let mut ev = event();
        ev.event_id = bad.to_string();
        let err = send("http://127.0.0.1:9/x", &ev, &opts).unwrap_err();
        assert!(matches!(err, WebhookError::InvalidEvent(_)), "{err:?}");
    }
}

#[test]
fn mask_url_keeps_only_scheme_and_host() {
    let cases = [
        (
            "https://hooks.example.com/services/T/B/xyz",
            "https://hooks.example.com/***",
        ),
        (
            "https://u:p@example.com:8443/a?b=c#d",
            "https://example.com:8443/***",
        ),
        ("http://example.com", "http://example.com"),
        ("http://example.com/", "http://example.com/***"),
        ("http://example.com?token=1", "http://example.com/***"),
        ("http://a@b@example.com/x", "http://example.com/***"),
        ("ftp://u:p@example.com/x", "ftp://example.com/***"),
        ("no scheme at all token=1", "<redacted url>"),
        ("javascript:alert(1)", "<redacted url>"),
    ];
    for (input, want) in cases {
        assert_eq!(mask_url(input), want, "{input}");
    }
}
