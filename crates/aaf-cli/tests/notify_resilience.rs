#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Resilience and edge-case tests for `aaf notify` (T3a).
//!
//! Runs the real `aaf` binary against local TCP sinks for webhooks and the
//! Telegram Bot API.

#[path = "factory_support.rs"]
mod support;

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::EventSink;
use serde_json::{Value, json};
use support::{Fixture, init_repo};

const TEST_TG_TOKEN: &str = "112233445:AAF_LocalNotifyMockToken_99887766";

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn emit_at(fx: &Fixture, kind: EventKind, task: &str, payload: Value, ts: i64) -> i64 {
    let event = FactoryEvent::new(kind, payload.to_string(), ts).for_task(task);
    fx.db().emit(&event).unwrap()
}

fn write_tier1(fx: &Fixture, extra: &str) {
    let tier1 = fx.root().join("home/.config/autofactory/config.toml");
    let mut cfg = fs::read_to_string(&tier1).unwrap();
    cfg.push('\n');
    cfg.push_str(extra);
    fs::write(&tier1, cfg).unwrap();
}

fn set_webhook(fx: &Fixture, base: &str) {
    write_tier1(
        fx,
        &format!("[notifications]\nwebhook_url = \"{base}/hook\"\n"),
    );
}

fn set_webhook_and_tg(fx: &Fixture, wh_base: &str, tg_base: &str) {
    write_tier1(
        fx,
        &format!(
            "[notifications]\nwebhook_url = \"{wh_base}/hook\"\n\n\
             [telegram]\nenabled = true\nallowed_chat_ids = [1001, 1002]\n\
             bot_token_env = \"AAF_TEST_TG_TOKEN\"\napi_base_url = \"{tg_base}\"\n"
        ),
    );
}

fn read_json_body(stream: &mut std::net::TcpStream) -> Value {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            len = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; len];
    let _ = reader.read_exact(&mut body);
    serde_json::from_slice(&body).unwrap_or(Value::Null)
}

fn http_sink<F>(responder: F) -> (String, mpsc::Receiver<Value>)
where
    F: Fn(&Value) -> (u16, String) + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let payload = read_json_body(&mut stream);
            let (status, body) = responder(&payload);
            let reason = match status {
                200 => "OK",
                403 => "Forbidden",
                404 => "Not Found",
                500 => "Internal Server Error",
                _ => "Status",
            };
            let resp = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
            let _ = tx.send(payload);
        }
    });
    (format!("http://{addr}"), rx)
}

fn drain(rx: &mpsc::Receiver<Value>) -> Vec<Value> {
    let mut items = Vec::new();
    while let Ok(v) = rx.recv_timeout(Duration::from_millis(200)) {
        items.push(v);
    }
    items
}

#[test]
fn case1_webhook_404_permanent_error_is_dropped_and_does_not_block_next_event() {
    let fx = Fixture::new();
    let (base, rx) = http_sink(|_| (404, String::new()));
    set_webhook(&fx, &base);
    let id = fx.add("task one");
    assert_eq!(fx.aaf(&["notify", "--once"]).code, 0);

    let old_ts = now_secs() - 300;
    let ev1 = emit_at(
        &fx,
        EventKind::TaskStateChanged,
        &id,
        json!({ "to": "blocked", "why": "budget exceeded" }),
        old_ts,
    );
    let ev2 = emit_at(
        &fx,
        EventKind::TaskStateChanged,
        &id,
        json!({ "to": "review" }),
        old_ts + 1,
    );

    let out = fx.aaf(&["notify", "--once"]);
    assert_ne!(
        out.code, 0,
        "dropped channel must exit != 0: {}",
        out.stderr
    );
    assert!(!out.json["errors"].as_array().unwrap().is_empty());
    assert_eq!(
        out.json["cursor"].as_i64(),
        Some(ev2),
        "ev1={ev1}, ev2={ev2}: {}",
        out.stderr
    );
    assert_eq!(drain(&rx).len(), 2);

    let again = fx.aaf(&["notify", "--once"]);
    assert_eq!(again.code, 0, "{}", again.stderr);
    assert_eq!(again.json["alerts"], json!([]), "{}", again.stderr);
    assert_eq!(again.json["errors"], json!([]), "{}", again.stderr);
    assert!(drain(&rx).is_empty());
}

#[test]
fn case2_webhook_500_retries_five_passes_then_drops_and_advances() {
    let fx = Fixture::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_srv = Arc::clone(&calls);
    let (base, rx) = http_sink(move |_| {
        if calls_srv.fetch_add(1, Ordering::SeqCst) < 5 {
            (500, String::new())
        } else {
            (200, String::new())
        }
    });
    set_webhook(&fx, &base);
    let id = fx.add("task retry");
    let init = fx.aaf(&["notify", "--once"]);
    assert_eq!(init.code, 0, "{}", init.stderr);
    let initial_cursor = init.json["cursor"].as_i64().unwrap();

    let ev1 = emit_at(
        &fx,
        EventKind::TaskStateChanged,
        &id,
        json!({ "to": "review" }),
        now_secs() - 300,
    );
    let ev2 = emit_at(
        &fx,
        EventKind::FactoryFrozen,
        &id,
        json!({ "consecutive_fails": 3, "threshold": 3 }),
        now_secs() - 299,
    );

    for pass_no in 1..=4 {
        let out = fx.aaf(&["notify", "--once"]);
        assert_ne!(out.code, 0, "pass {pass_no}: {}", out.stderr);
        assert_eq!(
            out.json["cursor"].as_i64(),
            Some(initial_cursor),
            "pass {pass_no} must hold cursor before {ev1}: {}",
            out.stderr
        );
    }

    let fifth = fx.aaf(&["notify", "--once"]);
    assert_ne!(fifth.code, 0, "pass 5 dropped ev1: {}", fifth.stderr);
    assert_eq!(fifth.json["cursor"].as_i64(), Some(ev2), "{}", fifth.stderr);

    let received = drain(&rx);
    assert_eq!(received.len(), 6, "5 retries + 1 success: {received:?}");
    assert_eq!(received[5]["status"], json!("frozen"));

    let sixth = fx.aaf(&["notify", "--once"]);
    assert_eq!(sixth.code, 0, "{}", sixth.stderr);
    assert_eq!(sixth.json["alerts"], json!([]), "{}", sixth.stderr);
}

#[test]
fn case3_successful_channels_are_not_resent_when_another_channel_fails() {
    // Part A: permanent 403 on chat A (1001), 200 on webhook and chat B (1002).
    let fx = Fixture::new();
    let (wh_base, wh_rx) = http_sink(|_| (200, String::new()));
    let (tg_base, tg_rx) = http_sink(|body| {
        if body["chat_id"].as_i64() == Some(1001) {
            (
                403,
                r#"{"ok":false,"error_code":403,"description":"Forbidden: bot was blocked"}"#
                    .to_string(),
            )
        } else {
            (200, r#"{"ok":true,"result":{"message_id":1}}"#.to_string())
        }
    });
    set_webhook_and_tg(&fx, &wh_base, &tg_base);
    let env = [("AAF_TEST_TG_TOKEN", TEST_TG_TOKEN)];
    let id = fx.add("multi channel");
    assert_eq!(fx.aaf_in(&fx.repo, &["notify", "--once"], &env).code, 0);

    let ev = emit_at(
        &fx,
        EventKind::TaskStateChanged,
        &id,
        json!({ "to": "review" }),
        now_secs() - 300,
    );

    let p1 = fx.aaf_in(&fx.repo, &["notify", "--once"], &env);
    assert_ne!(p1.code, 0, "{}", p1.stderr);
    assert_eq!(p1.json["cursor"].as_i64(), Some(ev), "{}", p1.stderr);
    assert_eq!(drain(&wh_rx).len(), 1);
    let ids: Vec<i64> = drain(&tg_rx)
        .iter()
        .filter_map(|v| v["chat_id"].as_i64())
        .collect();
    assert_eq!(ids, vec![1001, 1002]);

    let p2 = fx.aaf_in(&fx.repo, &["notify", "--once"], &env);
    assert_eq!(p2.code, 0, "{}", p2.stderr);
    assert!(drain(&wh_rx).is_empty());
    assert!(drain(&tg_rx).is_empty());

    // Part B: transient 500 on chat A on pass 1, 200 on pass 2; webhook & chat B are not re-sent.
    let fx2 = Fixture::new();
    let (wh_base2, wh_rx2) = http_sink(|_| (200, String::new()));
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_srv = Arc::clone(&hits);
    let (tg_base2, tg_rx2) = http_sink(move |body| {
        if body["chat_id"].as_i64() == Some(1001) && hits_srv.fetch_add(1, Ordering::SeqCst) == 0 {
            (
                500,
                r#"{"ok":false,"error_code":500,"description":"Internal Server Error"}"#
                    .to_string(),
            )
        } else {
            (200, r#"{"ok":true,"result":{"message_id":2}}"#.to_string())
        }
    });
    set_webhook_and_tg(&fx2, &wh_base2, &tg_base2);
    let id2 = fx2.add("partial retry");
    assert_eq!(fx2.aaf_in(&fx2.repo, &["notify", "--once"], &env).code, 0);
    let ev2 = emit_at(
        &fx2,
        EventKind::TaskStateChanged,
        &id2,
        json!({ "to": "review" }),
        now_secs() - 300,
    );

    let r1 = fx2.aaf_in(&fx2.repo, &["notify", "--once"], &env);
    assert_ne!(r1.code, 0, "{}", r1.stderr);
    assert_ne!(r1.json["cursor"].as_i64(), Some(ev2));
    assert_eq!(drain(&wh_rx2).len(), 1);
    assert_eq!(drain(&tg_rx2).len(), 2);

    let r2 = fx2.aaf_in(&fx2.repo, &["notify", "--once"], &env);
    assert_eq!(r2.code, 0, "{}", r2.stderr);
    assert_eq!(r2.json["cursor"].as_i64(), Some(ev2), "{}", r2.stderr);
    assert!(drain(&wh_rx2).is_empty(), "webhook must not be re-sent");
    let retry_ids: Vec<i64> = drain(&tg_rx2)
        .iter()
        .filter_map(|v| v["chat_id"].as_i64())
        .collect();
    assert_eq!(retry_ids, vec![1001], "only failed chat 1001 retried");
}

#[test]
fn case4_blocked_webhook_respects_factory_webhook_sent_and_grace_period() {
    let fx = Fixture::new();
    let (base, rx) = http_sink(|_| (200, String::new()));
    set_webhook(&fx, &base);
    let id = fx.add("budget task");
    assert_eq!(fx.aaf(&["notify", "--once"]).code, 0);

    // 4a: Blocked > 120s ago, NO WebhookSent -> notify posts to webhook.
    let old_ts = now_secs() - 300;
    let ev_old = emit_at(
        &fx,
        EventKind::TaskStateChanged,
        &id,
        json!({ "to": "blocked", "why": "budget exceeded" }),
        old_ts,
    );
    let out_a = fx.aaf(&["notify", "--once"]);
    assert_eq!(out_a.code, 0, "{}", out_a.stderr);
    assert_eq!(out_a.json["cursor"].as_i64(), Some(ev_old));
    let posts_a = drain(&rx);
    assert_eq!(posts_a.len(), 1, "expected webhook POST for Blocked");
    assert_eq!(posts_a[0]["status"], json!("blocked"));

    // 4b: Blocked > 120s ago WITH WebhookSent{ok:true} -> no duplicate webhook.
    emit_at(
        &fx,
        EventKind::TaskStateChanged,
        &id,
        json!({ "to": "blocked", "why": "factory already sent" }),
        old_ts + 1,
    );
    let ev_ws = emit_at(
        &fx,
        EventKind::WebhookSent,
        &id,
        json!({ "ok": true }),
        old_ts + 2,
    );
    let out_b = fx.aaf(&["notify", "--once"]);
    assert_eq!(out_b.code, 0, "{}", out_b.stderr);
    assert_eq!(out_b.json["cursor"].as_i64(), Some(ev_ws));
    assert!(drain(&rx).is_empty(), "factory already sent webhook");

    // 4c: Fresh Blocked (< 120s ago), NO WebhookSent -> grace holds webhook & cursor.
    let ev_fresh = emit_at(
        &fx,
        EventKind::TaskStateChanged,
        &id,
        json!({ "to": "blocked", "why": "just happened" }),
        now_secs(),
    );
    let out_c = fx.aaf(&["notify", "--once"]);
    assert_eq!(out_c.code, 0, "{}", out_c.stderr);
    assert_eq!(
        out_c.json["cursor"].as_i64(),
        Some(ev_ws),
        "cursor must stay before {ev_fresh}: {}",
        out_c.stderr
    );
    assert!(drain(&rx).is_empty(), "webhook must wait during grace");
}

#[test]
fn case5_repo_scoped_notify_sees_global_factory_frozen_from_other_repo() {
    let fx = Fixture::new();
    let repo_b = init_repo(&fx.root(), "repo_b");
    let (base, rx) = http_sink(|_| (200, String::new()));
    set_webhook(&fx, &base);

    let _id_a = fx.add("task in A");
    let add_b = fx.aaf_in(
        &repo_b,
        &[
            "board", "add", "--title", "task B", "--spec", "s", "--accept", "a",
        ],
        &[],
    );
    assert_eq!(add_b.code, 0, "{}", add_b.stderr);
    let id_b = add_b.json["task"]["id"].as_str().unwrap().to_string();

    let repo_a = fx.repo.to_string_lossy().into_owned();
    assert_eq!(fx.aaf(&["notify", "--repo", &repo_a, "--once"]).code, 0);

    emit_at(
        &fx,
        EventKind::FactoryFrozen,
        &id_b,
        json!({ "consecutive_fails": 3, "threshold": 3 }),
        now_secs() - 10,
    );

    let out = fx.aaf(&["notify", "--repo", &repo_a, "--once"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    let alerts = out.json["alerts"].as_array().unwrap();
    assert!(
        alerts.iter().any(|a| a["kind"] == json!("frozen")),
        "{}",
        out.stderr
    );
    let posts = drain(&rx);
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["status"], json!("frozen"));
}

#[test]
fn case6_missing_telegram_token_disables_telegram_with_warning_and_keeps_webhook() {
    let fx = Fixture::new();
    let (base, rx) = http_sink(|_| (200, String::new()));
    write_tier1(
        &fx,
        &format!(
            "[notifications]\nwebhook_url = \"{base}/hook\"\n\n\
             [telegram]\nenabled = true\nallowed_chat_ids = [1001]\n\
             bot_token_env = \"AAF_MISSING_TG_TOKEN\"\n"
        ),
    );
    let id = fx.add("webhook still works");
    let init = fx.aaf(&["notify", "--once"]);
    assert_eq!(init.code, 0, "{}", init.stderr);
    assert!(
        init.stderr.contains("telegram disabled for this run"),
        "{}",
        init.stderr
    );

    emit_at(
        &fx,
        EventKind::TaskStateChanged,
        &id,
        json!({ "to": "review" }),
        now_secs() - 10,
    );
    let out = fx.aaf(&["notify", "--once"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(
        out.stderr.contains("telegram disabled for this run"),
        "{}",
        out.stderr
    );
    assert_eq!(drain(&rx).len(), 1);
}

#[test]
fn case7_dry_run_first_pass_does_not_create_cursor_file() {
    let fx = Fixture::new();
    let _id = fx.add("dry run test");
    let key = aaf_fp::repo_id(&fx.repo);
    let cursor_file = fx.data.join("notify").join(format!("{key}.cursor"));

    let out = fx.aaf(&["notify", "--once", "--dry-run"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["alerts"], json!([]), "{}", out.stderr);
    assert!(
        out.json["cursor"].as_i64().unwrap_or(0) > 0,
        "{}",
        out.stderr
    );
    assert!(
        !cursor_file.exists(),
        "cursor created at {}",
        cursor_file.display()
    );
}

#[test]
fn case8_cursor_ahead_of_db_max_resets_with_warning() {
    let fx = Fixture::new();
    let (base, _rx) = http_sink(|_| (200, String::new()));
    set_webhook(&fx, &base);
    let _id = fx.add("db recreated");

    let key = aaf_fp::repo_id(&fx.repo);
    let notify_dir = fx.data.join("notify");
    fs::create_dir_all(&notify_dir).unwrap();
    let cursor_file = notify_dir.join(format!("{key}.cursor"));
    fs::write(&cursor_file, "1000000").unwrap();

    let max_id = fx
        .db()
        .max_event_id(Some(&fx.repo.to_string_lossy()))
        .unwrap();
    let out = fx.aaf(&["notify", "--once"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["cursor_reset"], json!(true), "{}", out.stderr);
    assert_eq!(out.json["cursor"].as_i64(), Some(max_id), "{}", out.stderr);
    assert!(out.stderr.contains("event log was reset"), "{}", out.stderr);
    let persisted: i64 = fs::read_to_string(&cursor_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(persisted, max_id);
}

#[test]
fn case9_second_notify_process_for_same_scope_is_rejected_by_lock() {
    let fx = Fixture::new();
    let (base, _rx) = http_sink(|_| (200, String::new()));
    set_webhook(&fx, &base);
    let _id = fx.add("lock test");

    let key = aaf_fp::repo_id(&fx.repo);
    let notify_dir = fx.data.join("notify");
    fs::create_dir_all(&notify_dir).unwrap();
    let lock_path = notify_dir.join(format!("{key}.lock"));
    let held = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .unwrap();
    held.try_lock().unwrap();

    let out = fx.aaf(&["notify", "--once"]);
    assert_ne!(out.code, 0, "{}", out.stderr);
    assert!(out.stderr.contains("another aaf notify"), "{}", out.stderr);

    let dry = fx.aaf(&["notify", "--once", "--dry-run"]);
    assert_eq!(dry.code, 0, "dry-run must ignore lock: {}", dry.stderr);
    drop(held);
}
