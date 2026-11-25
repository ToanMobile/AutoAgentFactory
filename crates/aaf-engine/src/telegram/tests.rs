#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};
use std::thread;
use std::time::Duration;

use aaf_domain::ports::Clock;
use aaf_domain::telegram::TelegramConfig;

use super::*;

struct FakeClock(AtomicI64);
impl FakeClock {
    fn new(now: i64) -> Self {
        Self(AtomicI64::new(now))
    }
    fn advance(&self, secs: i64) {
        self.0.fetch_add(secs, Ordering::SeqCst);
    }
}
impl Clock for FakeClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Default)]
struct MockApi {
    batches: Mutex<VecDeque<Vec<Update>>>,
    sent: Mutex<Vec<(i64, String)>>,
}
impl MockApi {
    fn with_batches(batches: Vec<Vec<Update>>) -> Self {
        Self {
            batches: Mutex::new(batches.into()),
            sent: Mutex::new(Vec::new()),
        }
    }
    fn sent(&self) -> Vec<(i64, String)> {
        self.sent.lock().unwrap().clone()
    }
}
impl TelegramApi for MockApi {
    fn get_updates(&self, _off: Option<i64>, _t: u64) -> Result<Vec<Update>, TelegramError> {
        Ok(self.batches.lock().unwrap().pop_front().unwrap_or_default())
    }
    fn send_message(&self, chat_id: i64, text: &str) -> Result<(), TelegramError> {
        self.sent.lock().unwrap().push((chat_id, text.to_string()));
        Ok(())
    }
}

#[derive(Default)]
struct MockHandler {
    calls: Vec<String>,
}
impl CommandHandler for MockHandler {
    fn status(&self, all: bool) -> Result<String, String> {
        Ok(format!("status(all={all})"))
    }
    fn board(&self, state: Option<&str>) -> Result<String, String> {
        Ok(format!("board(state={})", state.unwrap_or("all")))
    }
    fn show(&self, task_id: &str) -> Result<String, String> {
        Ok(format!("show({task_id})"))
    }
    fn handover(&self, task_id: &str) -> Result<String, String> {
        Ok(format!("handover({task_id})"))
    }
    fn preview_approve(&self, task_id: &str) -> Result<ApprovePreview, String> {
        Ok(ApprovePreview {
            task_id: task_id.to_string(),
            lock_fp: "deadbeef12345678".to_string(),
            locked_changes: vec![LockedFileSummary {
                change: "M".to_string(),
                path: "Cargo.toml".to_string(),
            }],
        })
    }
    fn approve(&mut self, id: &str, fp: &str, uid: i64) -> Result<String, String> {
        let msg = format!("approve({id},{fp},u={uid})");
        self.calls.push(msg.clone());
        Ok(msg)
    }
    fn unblock(&mut self, id: &str, uid: i64) -> Result<String, String> {
        let msg = format!("unblock({id},u={uid})");
        self.calls.push(msg.clone());
        Ok(msg)
    }
    fn unblock_all(&mut self, uid: i64) -> Result<String, String> {
        let msg = format!("unblock_all(u={uid})");
        self.calls.push(msg.clone());
        Ok(msg)
    }
    fn rework(&mut self, id: &str, reason: &str, uid: i64) -> Result<String, String> {
        let msg = format!("rework({id},{reason},u={uid})");
        self.calls.push(msg.clone());
        Ok(msg)
    }
    fn steer(&mut self, id: &str, note: &str, uid: i64) -> Result<String, String> {
        let msg = format!("steer({id},{note},u={uid})");
        self.calls.push(msg.clone());
        Ok(msg)
    }
    fn unfreeze(&mut self, uid: i64) -> Result<String, String> {
        let msg = format!("unfreeze(u={uid})");
        self.calls.push(msg.clone());
        Ok(msg)
    }
}

fn mk_cfg(allowed: Vec<i64>, allow_approve: bool, allow_mutating: bool) -> TelegramConfig {
    TelegramConfig {
        enabled: true,
        bot_token_env: "AAF_TELEGRAM_BOT_TOKEN".to_string(),
        allowed_chat_ids: allowed,
        allowed_user_ids: Vec::new(),
        poll_timeout_secs: 5,
        confirm_ttl_secs: 60,
        allow_approve,
        allow_mutating,
        api_base_url: "https://api.telegram.org".to_string(),
    }
}

fn mk_update(update_id: i64, chat_id: i64, user_id: Option<i64>, text: &str) -> Update {
    Update {
        update_id,
        message: Some(Message {
            message_id: update_id,
            from: user_id.map(|id| User {
                id,
                is_bot: false,
                username: Some("owner".to_string()),
                first_name: Some("Owner".to_string()),
            }),
            chat: Chat {
                id: chat_id,
                chat_type: "private".to_string(),
            },
            text: Some(text.to_string()),
            date: 1_700_000_000,
        }),
    }
}

fn reply_of(
    disp: &mut CommandDispatcher,
    handler: &mut MockHandler,
    clock: &FakeClock,
    upd: Update,
) -> String {
    match disp.handle_update(&upd, handler, clock).unwrap() {
        UpdateOutcome::Reply { text, .. } => text,
        other => panic!("expected Reply, got {other:?}"),
    }
}

fn extract_code(reply: &str) -> String {
    let idx = reply.find("/confirm ").expect("missing /confirm code");
    reply[idx + 9..]
        .split_whitespace()
        .next()
        .expect("code token")
        .to_string()
}

#[test]
fn empty_allowlist_and_disabled_are_fail_closed() {
    let clock = FakeClock::new(1000);
    let api = MockApi::default();
    let mut handler = MockHandler::default();

    let empty = mk_cfg(vec![], false, false);
    assert_eq!(
        validate_config(&empty, "123:tok"),
        Err(TelegramError::EmptyAllowlist)
    );
    assert_eq!(
        CommandDispatcher::new(empty.clone()).unwrap_err(),
        TelegramError::EmptyAllowlist
    );
    assert_eq!(
        run_poll_loop(&api, &empty, &mut handler, &clock, Some(1)),
        Err(TelegramError::EmptyAllowlist)
    );
    assert_eq!(
        send_alert_with(&api, &empty, "alert"),
        Err(TelegramError::EmptyAllowlist)
    );

    let mut disabled = mk_cfg(vec![42], false, false);
    disabled.enabled = false;
    assert_eq!(
        validate_config(&disabled, "123:tok"),
        Err(TelegramError::Disabled)
    );
    assert_eq!(
        run_poll_loop(&api, &disabled, &mut handler, &clock, Some(1)),
        Err(TelegramError::Disabled)
    );
    assert_eq!(
        send_alert_with(&api, &disabled, "alert"),
        Err(TelegramError::Disabled)
    );
    assert!(api.sent().is_empty());
}

#[test]
fn unauthorized_chat_is_silently_ignored_with_zero_calls_or_replies() {
    let cfg = mk_cfg(vec![1001], true, true);
    let clock = FakeClock::new(2000);
    let mut handler = MockHandler::default();
    let api = MockApi::with_batches(vec![vec![
        mk_update(1, 9999, Some(77), "/status"),
        mk_update(2, 9999, Some(77), "/approve t-1"),
        mk_update(3, 9999, Some(77), "/unfreeze"),
    ]]);

    let stats = run_poll_loop(&api, &cfg, &mut handler, &clock, Some(1)).unwrap();
    assert_eq!(stats.updates_seen, 3);
    assert_eq!(stats.ignored_unauthorized, 3);
    assert_eq!(stats.commands_handled, 0);
    assert_eq!(stats.replies_sent, 0);
    assert_eq!(stats.next_offset, Some(4));
    assert!(api.sent().is_empty());
    assert!(handler.calls.is_empty());
}

#[test]
fn read_only_commands_and_botname_suffix_work_for_authorized_chat() {
    let cfg = mk_cfg(vec![1001], false, false);
    let clock = FakeClock::new(3000);
    let mut handler = MockHandler::default();
    let api = MockApi::with_batches(vec![vec![
        mk_update(10, 1001, Some(1001), "/help@MyAafBot"),
        mk_update(11, 1001, Some(1001), "/status"),
        mk_update(12, 1001, Some(1001), "/status@MyAafBot all"),
        mk_update(13, 1001, Some(1001), "/board"),
        mk_update(14, 1001, Some(1001), "/board blocked"),
        mk_update(15, 1001, Some(1001), "/show t-abc123"),
        mk_update(16, 1001, Some(1001), "/handover t-abc123"),
        mk_update(17, 1001, Some(1001), "plain chat message ignored"),
    ]]);

    let stats = run_poll_loop(&api, &cfg, &mut handler, &clock, Some(1)).unwrap();
    assert_eq!(stats.commands_handled, 7);
    assert_eq!(stats.replies_sent, 7);
    let sent = api.sent();
    assert!(sent[0].1.contains("AAF Telegram Remote Companion"));
    assert_eq!(sent[1].1, "status(all=false)");
    assert_eq!(sent[2].1, "status(all=true)");
    assert_eq!(sent[3].1, "board(state=all)");
    assert_eq!(sent[4].1, "board(state=blocked)");
    assert_eq!(sent[5].1, "show(t-abc123)");
    assert_eq!(sent[6].1, "handover(t-abc123)");
}

#[test]
fn approve_and_mutating_rejected_when_disabled_in_config() {
    let cfg = mk_cfg(vec![1001], false, false);
    let clock = FakeClock::new(4000);
    let mut handler = MockHandler::default();
    let mut disp = CommandDispatcher::new(cfg).unwrap();

    for (id, cmd) in [
        (1, "/approve t-1"),
        (2, "/unblock t-1"),
        (3, "/rework t-1 fix tests"),
        (4, "/steer t-1 focus on parser"),
        (5, "/unfreeze"),
    ] {
        let reply = reply_of(
            &mut disp,
            &mut handler,
            &clock,
            mk_update(id, 1001, Some(1001), cmd),
        );
        assert!(reply.contains("refused:"), "{cmd} -> {reply}");
    }
    assert_eq!(disp.pending_len(), 0);
    assert!(handler.calls.is_empty());
}

#[test]
fn two_step_approve_succeeds_once_and_rejects_replay() {
    let cfg = mk_cfg(vec![1001], true, false);
    let clock = FakeClock::new(5000);
    let mut handler = MockHandler::default();
    let mut disp = CommandDispatcher::new(cfg).unwrap();

    let preview = reply_of(
        &mut disp,
        &mut handler,
        &clock,
        mk_update(1, 1001, Some(1001), "/approve t-42"),
    );
    assert!(preview.contains("deadbeef12345678"));
    assert!(preview.contains("M Cargo.toml"));
    assert!(handler.calls.is_empty());

    let code = extract_code(&preview);
    assert_eq!(code.len(), 8);

    let confirmed = reply_of(
        &mut disp,
        &mut handler,
        &clock,
        mk_update(2, 1001, Some(1001), &format!("/confirm {code}")),
    );
    assert_eq!(confirmed, "approve(t-42,deadbeef12345678,u=1001)");
    assert_eq!(handler.calls.len(), 1);

    // Replay of the exact same single-use code MUST fail.
    let replay = reply_of(
        &mut disp,
        &mut handler,
        &clock,
        mk_update(3, 1001, Some(1001), &format!("/confirm {code}")),
    );
    assert!(replay.contains("invalid or expired"));
    assert_eq!(handler.calls.len(), 1);
}

#[test]
fn two_step_mutating_commands_work_and_anonymous_or_bad_id_refused() {
    let cfg = mk_cfg(vec![1001], true, true);
    let clock = FakeClock::new(6000);
    let mut handler = MockHandler::default();
    let mut disp = CommandDispatcher::new(cfg).unwrap();

    // Anonymous sender (`from: None`) is silently rejected; path-traversal task_id is refused.
    assert_eq!(
        disp.handle_update(&mk_update(1, 1001, None, "/unfreeze"), &mut handler, &clock)
            .unwrap(),
        UpdateOutcome::IgnoredUnauthorized
    );
    let bad_id = reply_of(
        &mut disp,
        &mut handler,
        &clock,
        mk_update(2, 1001, Some(1001), "/show ../etc/passwd"),
    );
    assert!(bad_id.contains("invalid task id"));

    // Safe recovery commands execute in 1 step when allowed_chat_ids is non-empty.
    for (idx, (cmd, expected)) in [
        ("/unblock t-1", "unblock(t-1,u=1001)"),
        ("/unblock all", "unblock_all(u=1001)"),
        ("/unfreeze", "unfreeze(u=1001)"),
    ]
    .iter()
    .enumerate()
    {
        let done = reply_of(
            &mut disp,
            &mut handler,
            &clock,
            mk_update(10 + idx as i64, 1001, Some(1001), cmd),
        );
        assert_eq!(done, *expected);
    }
    assert_eq!(disp.pending_len(), 0);

    // Higher-impact commands still require 2-step `/confirm`.
    for (idx, (cmd, expected)) in [
        (
            "/rework t-2 add edge test",
            "rework(t-2,add edge test,u=1001)",
        ),
        ("/steer t-3 avoid alloc", "steer(t-3,avoid alloc,u=1001)"),
    ]
    .iter()
    .enumerate()
    {
        let base_id = (idx as i64) * 2 + 20;
        let preview = reply_of(
            &mut disp,
            &mut handler,
            &clock,
            mk_update(base_id, 1001, Some(1001), cmd),
        );
        let code = extract_code(&preview);
        let done = reply_of(
            &mut disp,
            &mut handler,
            &clock,
            mk_update(base_id + 1, 1001, Some(1001), &format!("/confirm {code}")),
        );
        assert_eq!(done, *expected);
    }
    assert_eq!(handler.calls.len(), 5);
}

#[test]
fn wrong_user_or_wrong_chat_cannot_confirm_another_users_nonce() {
    let mut cfg = mk_cfg(vec![1001, 1002], true, true);
    cfg.allowed_user_ids = vec![500, 501];
    let clock = FakeClock::new(7000);
    let mut handler = MockHandler::default();
    let mut disp = CommandDispatcher::new(cfg).unwrap();

    let preview = reply_of(
        &mut disp,
        &mut handler,
        &clock,
        mk_update(1, 1001, Some(500), "/approve t-9"),
    );
    let code = extract_code(&preview);

    // Same chat, different user_id -> rejected without consuming nonce.
    let wrong_user = reply_of(
        &mut disp,
        &mut handler,
        &clock,
        mk_update(2, 1001, Some(501), &format!("/confirm {code}")),
    );
    assert!(wrong_user.contains("invalid or expired"));

    // Different authorized chat, same user_id -> rejected without consuming nonce.
    let wrong_chat = reply_of(
        &mut disp,
        &mut handler,
        &clock,
        mk_update(3, 1002, Some(500), &format!("/confirm {code}")),
    );
    assert!(wrong_chat.contains("invalid or expired"));
    assert!(handler.calls.is_empty());

    // Original (chat_id, user_id) still succeeds.
    let ok = reply_of(
        &mut disp,
        &mut handler,
        &clock,
        mk_update(4, 1001, Some(500), &format!("/confirm {code}")),
    );
    assert_eq!(ok, "approve(t-9,deadbeef12345678,u=500)");
}

#[test]
fn expired_nonce_is_rejected_and_pending_store_is_bounded() {
    let cfg = mk_cfg(vec![1001], true, true);
    let clock = FakeClock::new(10_000);
    let mut handler = MockHandler::default();
    let mut disp = CommandDispatcher::new(cfg).unwrap();

    let preview = reply_of(
        &mut disp,
        &mut handler,
        &clock,
        mk_update(1, 1001, Some(1001), "/rework t-1 retry"),
    );
    let code = extract_code(&preview);

    clock.advance(61);
    let expired = reply_of(
        &mut disp,
        &mut handler,
        &clock,
        mk_update(2, 1001, Some(1001), &format!("/confirm {code}")),
    );
    assert!(expired.contains("invalid or expired"));
    assert!(handler.calls.is_empty());

    for i in 0..(MAX_PENDING_CONFIRMATIONS + 10) {
        let _ = reply_of(
            &mut disp,
            &mut handler,
            &clock,
            mk_update(100 + i as i64, 1001, Some(1001), "/rework t-1 retry"),
        );
    }
    assert_eq!(disp.pending_len(), MAX_PENDING_CONFIRMATIONS);
}

#[test]
fn bot_token_never_leaks_and_alert_deduplicates_and_sanitizes() {
    let secret = "987654321:AAHSuperSecretBotTokenXYZ123456";
    let api = HttpTelegramApi::new("https://api.telegram.org", secret).unwrap();
    assert!(!format!("{api:?}").contains(secret));

    for bad in [
        format!("ftp://api.telegram.org/bot{secret}/getUpdates"),
        "https://user:pass@api.telegram.org".to_string(),
        "https://api.telegram.org?x=1".to_string(),
    ] {
        let err = validate_base_url(&bad).unwrap_err();
        let text = format!("{err} | {err:?}");
        assert!(!text.contains(secret));
        assert!(!text.contains("pass"));
    }
    assert_eq!(
        validate_token("bad/token"),
        Err(TelegramError::InvalidToken)
    );

    let scrubbed = sanitize_output(&format!(
        "ok \x1b[31mred\x1b[0m\ntoken={secret}\nBearer eyJhbGciOiJIUzI1NiJ9.abcdef\ngh{}_1234567890abcdef",
        "p"
    ));
    assert!(!scrubbed.contains(secret));
    assert!(!scrubbed.contains("\x1b"));
    assert!(!scrubbed.contains("eyJhbGciOiJIUzI1NiJ9"));
    assert!(!scrubbed.contains("1234567890abcdef"));

    let mock = MockApi::default();
    let cfg = mk_cfg(vec![1001, 1002, 1001], false, false);
    send_alert_with(&mock, &cfg, "blocked \x1b[31mt-1\x1b[0m").unwrap();
    let sent = mock.sent();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0], (1001, "blocked t-1".to_string()));
    assert_eq!(sent[1], (1002, "blocked t-1".to_string()));
}

#[test]
fn truncate_message_respects_utf8_boundaries_within_4000_bytes() {
    let long = "Kiểm tra tiếng Việt 🚀 ".repeat(300);
    assert!(long.len() > MAX_MESSAGE_BYTES);
    let truncated = truncate_message(&long, MAX_MESSAGE_BYTES);
    assert!(truncated.len() <= MAX_MESSAGE_BYTES);
    assert!(truncated.ends_with("\n...(truncated)"));
    assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());

    let tiny = truncate_message("🚀🦀✅", 5);
    assert!(tiny.len() <= 5);
    assert_eq!(tiny, "🚀");
}

#[test]
fn http_telegram_api_works_against_local_tcp_mock_and_redacts_token_on_error() {
    let secret = "112233445:AAF_LocalMockToken_99887766";
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for i in 0..3 {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            requests.push(String::from_utf8_lossy(&buf[..n]).into_owned());

            let (status, body) = match i {
                0 => (
                    "200 OK",
                    r#"{"ok":true,"result":[{"update_id":7,"message":{"message_id":1,"from":{"id":55},"chat":{"id":1001,"type":"private"},"text":"/status","date":1700000000}}]}"#,
                ),
                1 => ("200 OK", r#"{"ok":true,"result":{"message_id":2}}"#),
                _ => (
                    "401 Unauthorized",
                    r#"{"ok":false,"error_code":401,"description":"Unauthorized for /bot112233445:AAF_LocalMockToken_99887766/sendMessage"}"#,
                ),
            };
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(resp.as_bytes()).unwrap();
            stream.flush().unwrap();
        }
        requests
    });

    let api = HttpTelegramApi::with_timeouts(
        &format!("http://{addr}"),
        secret,
        Duration::from_secs(2),
        Duration::from_secs(5),
    )
    .unwrap();

    let updates = api.get_updates(Some(6), 1).unwrap();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].update_id, 7);
    assert_eq!(
        updates[0].message.as_ref().and_then(|m| m.text.as_deref()),
        Some("/status")
    );

    api.send_message(1001, "hello from aaf").unwrap();

    let err = api.send_message(1001, "trigger 401").unwrap_err();
    let msg = format!("{err} | {err:?}");
    assert!(!msg.contains(secret), "error leaked bot token: {msg}");
    assert!(msg.contains("401"));

    let requests = server.join().unwrap();
    assert!(requests[0].starts_with(&format!("POST /bot{secret}/getUpdates HTTP/1.1")));
    assert!(requests[1].starts_with(&format!("POST /bot{secret}/sendMessage HTTP/1.1")));
}
