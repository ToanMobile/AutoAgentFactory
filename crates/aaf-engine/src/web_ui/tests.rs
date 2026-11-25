#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::qr::{format_info_bits, pairing_url, render_pairing_card, render_qr_ascii};
use super::server::{ParseError, read_http_request_with_timeout};
use super::{
    HTML_CSP, UiBackend, UiConfig, UiError, constant_time_eq, is_loopback_host, start_server,
    validate_ui_config,
};

const TEST_TOKEN: &str = "0123456789abcdef0123456789abcdef";

#[derive(Default)]
struct FakeBackend {
    approved: Mutex<Vec<(String, String, String)>>,
    actions: Mutex<Vec<(String, String, String)>>,
}

impl UiBackend for FakeBackend {
    fn status_json(&self) -> Result<Value, String> {
        Ok(json!({
            "scope": "/tmp/fake-repo",
            "frozen": false,
            "counts": { "total": 2, "review": 1, "blocked": 1 },
            "runs": { "total": 1, "running": 1 },
            "tokens_in": 1200,
            "tokens_out": 400,
            "estimated_cost_usd": 0.0096,
        }))
    }

    fn board_json(&self, state: Option<&str>) -> Result<Value, String> {
        Ok(json!({
            "filter": state,
            "tasks": [
                { "id": "t-1", "title": "Fix gate", "state": "review", "priority": "P1" }
            ]
        }))
    }

    fn task_detail_json(&self, task_id: &str) -> Result<Value, String> {
        Ok(json!({ "id": task_id, "title": "Fix gate", "state": "review" }))
    }

    fn handover_json(&self, task_id: &str) -> Result<Value, String> {
        Ok(json!({ "task_id": task_id, "status": "XONG", "effective_score": 92 }))
    }

    fn approve_preview_json(&self, task_id: &str) -> Result<Value, String> {
        Ok(json!({
            "task_id": task_id,
            "lock_fp": "fp-expected-123",
            "locked_changes": [{ "change": "M", "path": "Cargo.toml" }],
        }))
    }

    fn approve_commit(
        &self,
        task_id: &str,
        expected_lock_fp: &str,
        reviewer: &str,
    ) -> Result<Value, String> {
        if expected_lock_fp != "fp-expected-123" {
            return Err("lock_fp mismatch: diff changed".to_string());
        }
        self.approved.lock().unwrap().push((
            task_id.to_string(),
            expected_lock_fp.to_string(),
            reviewer.to_string(),
        ));
        Ok(json!({ "approved": true, "task_id": task_id, "reviewer": reviewer }))
    }

    fn mutate_task(&self, action: &str, task_id: &str, note: &str) -> Result<Value, String> {
        self.actions.lock().unwrap().push((
            action.to_string(),
            task_id.to_string(),
            note.to_string(),
        ));
        Ok(json!({ "action": action, "task_id": task_id, "note": note }))
    }

    fn stream_tail_lines(&self, run_id: &str, max_lines: usize) -> Result<Vec<String>, String> {
        let _ = max_lines;
        Ok(vec![
            format!("{{\"type\":\"start\",\"tool\":\"claude\",\"target\":\"{run_id}\"}}"),
            "{\"type\":\"tool_use\",\"tool\":\"Read\",\"target\":\"src/lib.rs\"}".to_string(),
        ])
    }

    fn meta_json(&self) -> Result<Value, String> {
        Ok(json!({ "repo_name": "fake-repo", "read_only": false, "version": "3.5.0" }))
    }
}

struct RawResp {
    status: u16,
    headers: HashMap<String, String>,
    body: String,
}

fn send_raw(addr: SocketAddr, request: &str) -> RawResp {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    stream.flush().unwrap();

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8(raw).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    RawResp {
        status,
        headers,
        body: body.to_string(),
    }
}

fn start_test_server(allow_mutating: bool) -> (super::UiServerHandle, Arc<FakeBackend>) {
    start_test_server_opts(allow_mutating, false)
}

fn start_test_server_opts(
    allow_mutating: bool,
    allow_approve: bool,
) -> (super::UiServerHandle, Arc<FakeBackend>) {
    let backend = Arc::new(FakeBackend::default());
    let cfg = UiConfig {
        host: "127.0.0.1".to_string(),
        port: 0,
        token: TEST_TOKEN.to_string(),
        allow_mutating,
        allow_approve,
        lan_mode: false,
    };
    let handle = start_server(cfg, backend.clone()).unwrap();
    (handle, backend)
}

#[test]
fn non_loopback_host_without_strong_key_is_refused() {
    assert!(is_loopback_host("127.0.0.1"));
    assert!(is_loopback_host("localhost"));
    assert!(is_loopback_host("::1"));
    assert!(is_loopback_host("[::1]"));
    assert!(!is_loopback_host("0.0.0.0"));
    assert!(!is_loopback_host("192.168.1.10"));
    assert!(!is_loopback_host("::"));

    let empty = UiConfig::resolve("0.0.0.0", 4319, None, true);
    assert!(matches!(empty, Err(UiError::InsecureLanBind { .. })));

    let short = UiConfig::resolve("192.168.1.50", 4319, Some("too-short".into()), true);
    assert!(matches!(short, Err(UiError::InsecureLanBind { .. })));

    let manual = UiConfig {
        host: "0.0.0.0".to_string(),
        port: 4319,
        token: String::new(),
        allow_mutating: false,
        allow_approve: false,
        lan_mode: true,
    };
    assert!(matches!(
        validate_ui_config(&manual),
        Err(UiError::InsecureLanBind { .. })
    ));
}

#[test]
fn non_loopback_host_with_strong_key_and_loopback_autogen_are_accepted() {
    let lan = UiConfig::resolve(
        "192.168.1.50",
        4319,
        Some("strong-lan-secret-token-1234".into()),
        true,
    )
    .unwrap();
    assert!(lan.lan_mode);
    assert!(!lan.allow_approve);
    assert_eq!(lan.token, "strong-lan-secret-token-1234");

    let local = UiConfig::resolve("127.0.0.1", 4319, None, true).unwrap();
    assert!(!local.lan_mode);
    assert!(!local.allow_approve);
    assert_eq!(local.token.len(), 32);
    assert!(local.token.bytes().all(|b| b.is_ascii_hexdigit()));

    assert!(constant_time_eq(b"secret-token", b"secret-token"));
    assert!(!constant_time_eq(b"secret-token", b"secret-tokeN"));
    assert!(!constant_time_eq(b"short", b"secret-token"));
}

#[test]
fn index_returns_html_with_strict_csp_and_security_headers() {
    let (handle, _) = start_test_server(true);
    let addr = handle.addr();
    let resp = send_raw(addr, &format!("GET / HTTP/1.1\r\nHost: {addr}\r\n\r\n"));
    assert_eq!(resp.status, 200);
    assert!(resp.body.contains("<!DOCTYPE html>"));
    assert!(resp.body.contains("AutoAgentFactory Cockpit"));
    assert_eq!(
        resp.headers
            .get("content-security-policy")
            .map(String::as_str),
        Some(HTML_CSP)
    );
    assert_eq!(
        resp.headers.get("x-frame-options").map(String::as_str),
        Some("DENY")
    );
    assert_eq!(
        resp.headers
            .get("x-content-type-options")
            .map(String::as_str),
        Some("nosniff")
    );
    assert_eq!(
        resp.headers.get("referrer-policy").map(String::as_str),
        Some("no-referrer")
    );
    assert_eq!(
        resp.headers.get("cache-control").map(String::as_str),
        Some("no-store")
    );
    handle.join();
}

#[test]
fn health_endpoint_needs_no_auth_and_leaks_no_repo_data() {
    let (handle, _) = start_test_server(true);
    let addr = handle.addr();
    let resp = send_raw(
        addr,
        &format!("GET /api/health HTTP/1.1\r\nHost: {addr}\r\n\r\n"),
    );
    assert_eq!(resp.status, 200);
    let json: Value = serde_json::from_str(&resp.body).unwrap();
    assert_eq!(json, json!({ "ok": true }));
    assert!(!resp.body.contains("fake-repo"));
    assert!(!resp.body.contains(TEST_TOKEN));
    assert_eq!(
        resp.headers.get("x-frame-options").map(String::as_str),
        Some("DENY")
    );
    handle.join();
}

#[test]
fn status_requires_valid_bearer_token_and_rejects_query_token() {
    let (handle, _) = start_test_server(true);
    let addr = handle.addr();

    let no_auth = send_raw(
        addr,
        &format!("GET /api/status HTTP/1.1\r\nHost: {addr}\r\n\r\n"),
    );
    assert_eq!(no_auth.status, 401);

    let wrong = send_raw(
        addr,
        &format!(
            "GET /api/status HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer wrong-token\r\n\r\n"
        ),
    );
    assert_eq!(wrong.status, 401);

    let query_only = send_raw(
        addr,
        &format!("GET /api/status?token={TEST_TOKEN} HTTP/1.1\r\nHost: {addr}\r\n\r\n"),
    );
    assert_eq!(query_only.status, 401);

    let ok = send_raw(
        addr,
        &format!(
            "GET /api/status HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {TEST_TOKEN}\r\n\r\n"
        ),
    );
    assert_eq!(ok.status, 200);
    let body: Value = serde_json::from_str(&ok.body).unwrap();
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["scope"], json!("/tmp/fake-repo"));
    handle.join();
}

#[test]
fn csrf_origin_and_dns_rebinding_host_headers_are_blocked() {
    let (handle, _) = start_test_server(true);
    let addr = handle.addr();
    let port = addr.port();

    let evil_origin = send_raw(
        addr,
        &format!(
            "GET /api/status HTTP/1.1\r\n\
             Host: 127.0.0.1:{port}\r\n\
             Origin: https://evil.example\r\n\
             Authorization: Bearer {TEST_TOKEN}\r\n\r\n"
        ),
    );
    assert_eq!(evil_origin.status, 403);

    let evil_host = send_raw(
        addr,
        &format!(
            "GET /api/status HTTP/1.1\r\n\
             Host: evil.example:{port}\r\n\
             Authorization: Bearer {TEST_TOKEN}\r\n\r\n"
        ),
    );
    assert_eq!(evil_host.status, 403);

    let same_origin = send_raw(
        addr,
        &format!(
            "GET /api/status HTTP/1.1\r\n\
             Host: 127.0.0.1:{port}\r\n\
             Origin: http://127.0.0.1:{port}\r\n\
             Authorization: Bearer {TEST_TOKEN}\r\n\r\n"
        ),
    );
    assert_eq!(same_origin.status, 200);
    handle.join();
}

#[test]
fn approve_post_verifies_lock_fp_and_respects_read_only_mode() {
    let good_body = r#"{"task_id":"t-1","lock_fp":"fp-expected-123","reviewer":"alice"}"#;
    let post = |addr: SocketAddr, body: &str| {
        send_raw(
            addr,
            &format!(
                "POST /api/approve HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {TEST_TOKEN}\r\n\
                 Content-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        )
    };

    // Default (allow_approve=false) must refuse POST /api/approve with 403.
    let (def_handle, def_backend) = start_test_server(true);
    let def_refused = post(def_handle.addr(), good_body);
    assert_eq!(def_refused.status, 403);
    assert!(def_backend.approved.lock().unwrap().is_empty());
    def_handle.join();

    // Opt-in allow_approve=true verifies lock_fp and signs.
    let (handle, backend) = start_test_server_opts(true, true);
    assert_eq!(post(handle.addr(), good_body).status, 200);
    assert_eq!(backend.approved.lock().unwrap().len(), 1);
    let bad_fp = r#"{"task_id":"t-1","lock_fp":"stale-fp","reviewer":"alice"}"#;
    assert_eq!(post(handle.addr(), bad_fp).status, 400);
    handle.join();

    // Read-only mode must refuse POST /api/approve even when allow_approve=true.
    let (ro_handle, ro_backend) = start_test_server_opts(false, true);
    assert_eq!(post(ro_handle.addr(), good_body).status, 403);
    assert!(ro_backend.approved.lock().unwrap().is_empty());
    ro_handle.join();
}

#[test]
fn action_post_refuses_query_token_and_read_only_mode() {
    let (handle, backend) = start_test_server(true);
    let addr = handle.addr();
    let payload = r#"{"action":"steer","task_id":"t-1","note":"check edge case"}"#;

    // Query-string token MUST NEVER be accepted on mutating POST endpoints.
    let query_rejected = send_raw(
        addr,
        &format!(
            "POST /api/action?token={TEST_TOKEN} HTTP/1.1\r\n\
             Host: {addr}\r\n\
             Content-Length: {}\r\n\r\n{payload}",
            payload.len()
        ),
    );
    assert_eq!(query_rejected.status, 401);
    assert!(backend.actions.lock().unwrap().is_empty());

    let ok = send_raw(
        addr,
        &format!(
            "POST /api/action HTTP/1.1\r\n\
             Host: {addr}\r\n\
             Authorization: Bearer {TEST_TOKEN}\r\n\
             Content-Length: {}\r\n\r\n{payload}",
            payload.len()
        ),
    );
    assert_eq!(ok.status, 200);
    assert_eq!(backend.actions.lock().unwrap().len(), 1);
    handle.join();

    let (ro_handle, _) = start_test_server(false);
    let ro_addr = ro_handle.addr();
    let ro_refused = send_raw(
        ro_addr,
        &format!(
            "POST /api/action HTTP/1.1\r\n\
             Host: {ro_addr}\r\n\
             Authorization: Bearer {TEST_TOKEN}\r\n\
             Content-Length: {}\r\n\r\n{payload}",
            payload.len()
        ),
    );
    assert_eq!(ro_refused.status, 403);
    ro_handle.join();
}

#[test]
fn sse_events_accepts_query_token_and_clean_shutdown_unblocks() {
    let (handle, _) = start_test_server(true);
    let addr = handle.addr();

    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(4)))
        .unwrap();
    let req = format!(
        "GET /api/events?token={TEST_TOKEN}&run_id=run-42 HTTP/1.1\r\nHost: {addr}\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).unwrap();
    stream.flush().unwrap();

    let mut collected = String::new();
    let mut buf = [0u8; 2048];
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline
        && (collected.matches(": keepalive").count() < 2
            || !(collected.contains("event: stream") && collected.contains("run-42")))
    {
        let n = stream.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        collected.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    assert!(collected.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(collected.contains("text/event-stream"));
    assert!(
        collected.matches("event: status").count() >= 2,
        "{collected}"
    );
    assert_eq!(collected.matches("event: stream").count(), 1, "{collected}");
    assert!(collected.contains("run-42"));

    let t0 = Instant::now();
    handle.join();
    assert!(
        t0.elapsed() < Duration::from_millis(500),
        "condvar shutdown should unblock listener promptly"
    );
}

#[test]
fn read_routes_and_error_codes_behave_cleanly() {
    let (handle, _) = start_test_server(true);
    let addr = handle.addr();
    let auth = format!("Authorization: Bearer {TEST_TOKEN}");

    for path in [
        "/api/meta",
        "/api/board?state=review",
        "/api/task/t-1",
        "/api/handover/t-1",
        "/api/approve/preview/t-1",
        "/api/streams/run-9",
    ] {
        let r = send_raw(
            addr,
            &format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\n{auth}\r\n\r\n"),
        );
        assert_eq!(r.status, 200, "path={path} body={}", r.body);
    }

    let bad_id = send_raw(
        addr,
        &format!("GET /api/task/..%2Fetc HTTP/1.1\r\nHost: {addr}\r\n{auth}\r\n\r\n"),
    );
    assert_eq!(bad_id.status, 400);

    let not_found = send_raw(
        addr,
        &format!("GET /api/nope HTTP/1.1\r\nHost: {addr}\r\n{auth}\r\n\r\n"),
    );
    assert_eq!(not_found.status, 404);

    let wrong_method = send_raw(
        addr,
        &format!(
            "POST /api/status HTTP/1.1\r\nHost: {addr}\r\n{auth}\r\nContent-Length: 0\r\n\r\n"
        ),
    );
    assert_eq!(wrong_method.status, 405);
    handle.join();
}

#[test]
fn qr_encoder_and_pairing_url_use_fragment_and_valid_spec_bits() {
    assert_eq!(format_info_bits(0), 0x77C4);
    let url = pairing_url("127.0.0.1", 4319, TEST_TOKEN);
    assert_eq!(url, format!("http://127.0.0.1:4319/#token={TEST_TOKEN}"));
    let ipv6 = pairing_url("::1", 4319, TEST_TOKEN);
    assert_eq!(ipv6, format!("http://[::1]:4319/#token={TEST_TOKEN}"));

    let qr = render_qr_ascii(&url).expect("65-byte pairing URL fits V4 QR");
    assert!(qr.contains('█'));
    assert!(qr.lines().count() >= 12);

    let too_long = "x".repeat(200);
    assert!(render_qr_ascii(&too_long).is_none());

    let card = render_pairing_card(&url, false, false, true);
    assert!(card.contains(&url));
    assert!(card.contains("loopback"));
}

#[test]
fn slowloris_trickle_headers_hit_total_deadline_408() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let client = thread::spawn(move || {
        let mut s = TcpStream::connect(addr).unwrap();
        for b in b"GET / HTTP/1.1\r\nX-Slow: aaaaaaaaaaaaaaaa" {
            if s.write_all(&[*b]).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }
    });
    let (mut srv, _) = listener.accept().unwrap();
    let res = read_http_request_with_timeout(&mut srv, Duration::from_millis(120));
    assert!(matches!(res, Err(ParseError::Status(408, _))));
    let _ = client.join();
}
