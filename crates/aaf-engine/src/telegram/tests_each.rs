#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use aaf_domain::telegram::TelegramConfig;

use super::client::{TelegramApi, TelegramError, Update, send_alert_each, send_alert_each_with};

struct FakeApi {
    outcomes: HashMap<i64, Result<(), TelegramError>>,
    sent: Mutex<Vec<(i64, String)>>,
}

impl FakeApi {
    fn new(outcomes: Vec<(i64, Result<(), TelegramError>)>) -> Self {
        Self {
            outcomes: outcomes.into_iter().collect(),
            sent: Mutex::new(Vec::new()),
        }
    }
}

impl TelegramApi for FakeApi {
    fn get_updates(
        &self,
        _offset: Option<i64>,
        _timeout_secs: u64,
    ) -> Result<Vec<Update>, TelegramError> {
        Ok(Vec::new())
    }

    fn send_message(&self, chat_id: i64, text: &str) -> Result<(), TelegramError> {
        self.sent.lock().unwrap().push((chat_id, text.to_string()));
        self.outcomes.get(&chat_id).cloned().unwrap_or(Ok(()))
    }
}

fn base_cfg(allowed: Vec<i64>) -> TelegramConfig {
    TelegramConfig {
        enabled: true,
        bot_token_env: "AAF_TELEGRAM_BOT_TOKEN".into(),
        allowed_chat_ids: allowed,
        poll_timeout_secs: 0,
        confirm_ttl_secs: 60,
        allow_approve: false,
        allow_mutating: false,
        api_base_url: "https://api.telegram.org".into(),
    }
}

#[test]
fn send_alert_each_with_isolates_per_chat_failures_and_filters_allowlist() {
    let cfg = base_cfg(vec![1001, 1002]);
    let api = FakeApi::new(vec![
        (
            1001,
            Err(TelegramError::ApiError {
                method: "sendMessage",
                code: Some(403),
                description: "Forbidden: bot was blocked".into(),
            }),
        ),
        (1002, Ok(())),
    ]);

    // 9999 is outside allowlist; 1002 is duplicated.
    let res =
        send_alert_each_with(&api, &cfg, "hello\x1b[31m alert", &[1001, 9999, 1002, 1002]).unwrap();
    assert_eq!(res.len(), 2);
    assert_eq!(res[0].0, 1001);
    assert!(matches!(
        res[0].1,
        Err(TelegramError::ApiError {
            code: Some(403),
            ..
        })
    ));
    assert_eq!(res[1], (1002, Ok(())));

    let sent = api.sent.lock().unwrap();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].0, 1001);
    assert_eq!(sent[1].0, 1002);
    assert!(!sent[1].1.contains('\x1b'));
}

#[test]
fn send_alert_each_with_rejects_disabled_and_empty_allowlist() {
    let api = FakeApi::new(vec![]);
    let mut disabled = base_cfg(vec![1001]);
    disabled.enabled = false;
    assert_eq!(
        send_alert_each_with(&api, &disabled, "hi", &[1001]),
        Err(TelegramError::Disabled)
    );

    let empty = base_cfg(vec![]);
    assert_eq!(
        send_alert_each_with(&api, &empty, "hi", &[1001]),
        Err(TelegramError::EmptyAllowlist)
    );
    assert!(api.sent.lock().unwrap().is_empty());
}

#[test]
fn send_alert_each_http_mock_returns_per_chat_outcomes_and_redacts_token() {
    let secret = "112233445:AAF_LocalEachMock_99887766";
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let server = thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            // Read the whole request: under load the body can arrive after
            // the headers, and one read() then misses `chat_id`.
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&raw);
                if let Some(end) = text.find("\r\n\r\n") {
                    let len = text[..end]
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    if raw.len() >= end + 4 + len {
                        break;
                    }
                }
            }
            let req = String::from_utf8_lossy(&raw).into_owned();
            let (status, body) = if req.contains("\"chat_id\":1001") {
                (
                    "403 Forbidden",
                    r#"{"ok":false,"error_code":403,"description":"Forbidden for 112233445:AAF_LocalEachMock_99887766"}"#,
                )
            } else {
                ("200 OK", r#"{"ok":true,"result":{"message_id":7}}"#)
            };
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(resp.as_bytes()).unwrap();
            stream.flush().unwrap();
        }
    });

    let mut cfg = base_cfg(vec![1001, 1002]);
    cfg.api_base_url = format!("http://{addr}");

    // Malformed token fails fast before any network call.
    assert_eq!(
        send_alert_each(&cfg, "bad/token", "hi", &[1001]),
        Err(TelegramError::InvalidToken)
    );

    let outcomes = send_alert_each(&cfg, secret, "alert text", &[1001, 9999, 1002]).unwrap();
    server.join().unwrap();

    assert_eq!(outcomes.len(), 2);
    assert_eq!(outcomes[0].0, 1001);
    let err = outcomes[0].1.as_ref().unwrap_err();
    let rendered = format!("{err} | {err:?}");
    assert!(!rendered.contains(secret), "token leaked: {rendered}");
    assert!(matches!(
        err,
        TelegramError::ApiError {
            code: Some(403),
            ..
        }
    ));
    assert_eq!(outcomes[1], (1002, Ok(())));
}
