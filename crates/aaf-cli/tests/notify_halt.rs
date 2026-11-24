#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Integration tests for `FactoryHalted` notification delivery and
//! `aaf notify --message` (T3b).

#[path = "factory_support.rs"]
mod support;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{Value, json};
use support::{Fixture, git};

fn http_sink(status: u16) -> (String, mpsc::Receiver<Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
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
            let reason = if status == 200 { "OK" } else { "Not Found" };
            let resp = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
            let _ = tx.send(serde_json::from_slice(&body).unwrap_or(Value::Null));
        }
    });
    (url, rx)
}

fn set_webhook(fx: &Fixture, url: &str) {
    let tier1 = fx.root().join("home/.config/autofactory/config.toml");
    let mut global = fs::read_to_string(&tier1).unwrap();
    global.push_str(&format!("\n[notifications]\nwebhook_url = \"{url}\"\n"));
    fs::write(&tier1, global).unwrap();
}

/// Case 4: A real factory halt records `factory_halted` (and `halt_recorded: true`
/// in `aaf run` JSON), and `aaf notify --once` pushes a webhook alert with
/// kind `halted`.
#[test]
fn factory_halted_event_triggers_webhook_alert_kind_halted() {
    let fx = Fixture::new();
    let dir = fx.repo.join(".autofactory");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("config.toml"),
        "[commands]\ntest_scoped = \"true\"\n\n[run]\nexecutor = \"custom\"\n\
         executor_cmd = [\"/bin/sh\", \"-c\", \"echo 'HTTP 401 unauthorized: not logged in' >&2; exit 1\", \"{prompt}\"]\n",
    )
    .unwrap();
    git(&fx.repo, &["add", "-A"]);
    git(
        &fx.repo,
        &["commit", "--quiet", "-m", "configure auth-failing executor"],
    );

    let (url, rx) = http_sink(200);
    set_webhook(&fx, &url);
    let id = fx.add("task that halts");
    // Seed notify cursor at current end of log.
    assert_eq!(fx.aaf(&["notify", "--once"]).code, 0);

    let run = fx.aaf(&["run", "--unsandboxed"]);
    assert_eq!(run.code, 2, "{}", run.stderr);
    assert_eq!(run.json["status"], json!("halted"), "{}", run.stderr);
    assert_eq!(run.json["halt_recorded"], json!(true), "{}", run.stderr);

    let out = fx.aaf(&["notify", "--once"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    let alerts = out.json["alerts"].as_array().unwrap();
    assert_eq!(alerts.len(), 1, "{}", out.stderr);
    assert_eq!(alerts[0]["kind"], json!("halted"), "{}", out.stderr);
    assert_eq!(alerts[0]["task_id"], json!(id), "{}", out.stderr);

    let posted = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(posted["status"], json!("halted"), "{posted}");
    assert_eq!(posted["kind"], json!("factory_halted"), "{posted}");
    assert_eq!(posted["task_id"], json!(id), "{posted}");
}

/// Case 5: `aaf notify --message` delivers once without touching cursor/pending/lock,
/// exits 0 on success, exits non-zero on HTTP 404, and works even if the repo's
/// `.autofactory/config.toml` has broken TOML syntax.
#[test]
fn notify_message_sends_once_without_cursor_and_survives_broken_repo_config() {
    let fx = Fixture::new();
    let (url, rx) = http_sink(200);
    set_webhook(&fx, &url);

    // Corrupt repo config.toml must NOT prevent `--message` from reading global config.
    let repo_cfg_dir = fx.repo.join(".autofactory");
    fs::create_dir_all(&repo_cfg_dir).unwrap();
    fs::write(repo_cfg_dir.join("config.toml"), "[[[broken toml syntax\n").unwrap();

    let out = fx.aaf(&["notify", "--message", "hello from supervisor"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    let posted = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(posted["status"], json!("message"), "{posted}");
    assert_eq!(
        posted["summary"],
        json!("hello from supervisor"),
        "{posted}"
    );
    assert!(
        posted["event_id"]
            .as_str()
            .unwrap_or("")
            .starts_with("aaf-message-"),
        "{posted}"
    );
    assert!(
        rx.recv_timeout(Duration::from_millis(200)).is_err(),
        "must send only once"
    );
    // No DB, cursor, pending, or lock files created in data_dir/notify.
    assert!(
        !fx.data.join("notify").exists(),
        "notify --message must not create cursor/lock dir"
    );

    // Dry run prints without sending.
    let dry = fx.aaf(&["notify", "--dry-run", "--message", "preview only"]);
    assert_eq!(dry.code, 0, "{}", dry.stderr);
    assert!(
        rx.recv_timeout(Duration::from_millis(200)).is_err(),
        "dry-run must not send"
    );

    // HTTP 404 -> non-zero exit code.
    let (url404, rx404) = http_sink(404);
    let tier1 = fx.root().join("home/.config/autofactory/config.toml");
    fs::write(
        &tier1,
        format!("[machine]\nmax_load_percent = 0\n\n[notifications]\nwebhook_url = \"{url404}\"\n"),
    )
    .unwrap();
    let fail = fx.aaf(&["notify", "--message", "will fail"]);
    assert_ne!(fail.code, 0, "404 must exit non-zero: {}", fail.stderr);
    let _ = rx404.recv_timeout(Duration::from_secs(5)).unwrap();
}

/// Case 6: `--message` conflicts with `--once` and `--interval` in clap.
#[test]
fn notify_message_conflicts_with_once_and_interval() {
    let fx = Fixture::new();
    let with_once = fx.aaf(&["notify", "--message", "hi", "--once"]);
    assert_ne!(with_once.code, 0, "{}", with_once.stderr);
    assert!(
        with_once.stderr.contains("cannot be used with"),
        "expected clap conflict error, got: {}",
        with_once.stderr
    );

    let with_interval = fx.aaf(&["notify", "--message", "hi", "--interval", "5"]);
    assert_ne!(with_interval.code, 0, "{}", with_interval.stderr);
    assert!(
        with_interval.stderr.contains("cannot be used with"),
        "expected clap conflict error, got: {}",
        with_interval.stderr
    );
}
