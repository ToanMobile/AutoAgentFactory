#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `aaf notify` through the real binary: it pushes "needs you" moments
//! (ready for review, blocked, factory frozen) once each, from a cursor that
//! starts at the end of the log.

#[path = "factory_support.rs"]
mod support;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::EventSink;
use serde_json::{Value, json};
use support::{Fixture, git};

fn configure(fx: &Fixture) {
    configure_with_test(fx, "ls made-*.txt");
}

/// An empty `test` leaves the gate Unverified, which blocks the task for a
/// person: a real `blocked` moment.
fn configure_with_test(fx: &Fixture, test: &str) {
    let dir = fx.repo.join(".autofactory");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("config.toml"),
        format!(
            "[commands]\ntest_scoped = {}\n\n[run]\nexecutor = \"custom\"\n\
             executor_cmd = [\"/bin/sh\", \"-c\", \"echo made > made-$$.txt\", \"{{prompt}}\"]\n",
            json!(test)
        ),
    )
    .unwrap();
    git(&fx.repo, &["add", "-A"]);
    git(&fx.repo, &["commit", "--quiet", "-m", "configure aaf"]);
}

fn kinds(out: &Value) -> Vec<String> {
    out["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| format!("{}:{}", a["kind"].as_str().unwrap(), a["task_id"]))
        .collect()
}

fn emit(fx: &Fixture, kind: EventKind, task: &str, payload: Value) {
    let event = FactoryEvent::new(kind, payload.to_string(), 1).for_task(task);
    fx.db().emit(&event).unwrap();
}

fn seed_cursor(fx: &Fixture, scope: Option<&str>, key: &str) {
    let max_id = fx.db().max_event_id(scope).unwrap();
    let dir = fx.data.join("notify");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(format!("{key}.cursor")), max_id.to_string()).unwrap();
}

#[test]
fn dry_run_lists_new_needs_you_moments_without_moving_the_cursor() {
    let fx = Fixture::new();
    configure(&fx);
    let id = fx.add("make a file");
    // First pass only marks where the log ends: history is not replayed.
    let first = fx.aaf(&["notify", "--once", "--dry-run"]);
    assert_eq!(first.code, 0, "{}", first.stderr);
    assert_eq!(first.json["alerts"], json!([]), "{}", first.stderr);
    seed_cursor(
        &fx,
        Some(&fx.repo.to_string_lossy()),
        &aaf_fp::repo_id(&fx.repo),
    );

    assert_eq!(fx.aaf(&["run", "--unsandboxed"]).code, 0);
    emit(
        &fx,
        EventKind::TaskStateChanged,
        &id,
        json!({ "to": "blocked", "why": "head-lock needs a reviewer" }),
    );
    emit(
        &fx,
        EventKind::FactoryFrozen,
        &id,
        json!({ "consecutive_fails": 3, "threshold": 3 }),
    );

    let out = fx.aaf(&["notify", "--once", "--dry-run"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    let quoted = format!("\"{id}\"");
    assert_eq!(
        kinds(&out.json),
        [
            format!("review:{quoted}"),
            format!("blocked:{quoted}"),
            format!("frozen:{quoted}")
        ],
        "{}",
        out.stderr
    );
    let texts: Vec<&str> = out.json["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["text"].as_str().unwrap())
        .collect();
    assert!(texts[0].contains("make a file") && texts[0].contains("ready for review"));
    assert!(texts[1].contains("head-lock needs a reviewer"));
    assert!(texts[2].contains("aaf board unfreeze"));
    // A dry run sends nothing and keeps the cursor: the same moments again.
    let again = fx.aaf(&["notify", "--once", "--dry-run"]);
    assert_eq!(kinds(&again.json).len(), 3, "{}", again.stderr);
}

#[test]
fn without_a_channel_a_real_run_refuses_instead_of_dropping_alerts() {
    let fx = Fixture::new();
    configure(&fx);
    fx.add("make a file");
    let out = fx.aaf(&["notify", "--once"]);
    assert_ne!(out.code, 0, "{}", out.stderr);
    assert!(
        out.stderr.contains("no notification channel"),
        "{}",
        out.stderr
    );
}

/// Accepts webhook POSTs on 127.0.0.1 and hands each JSON body to `tx`.
fn webhook_sink() -> (String, mpsc::Receiver<Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut len = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                let lower = line.to_ascii_lowercase();
                if let Some(v) = lower.strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; len];
            reader.read_exact(&mut body).unwrap();
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            let _ = tx.send(serde_json::from_slice(&body).unwrap_or(Value::Null));
        }
    });
    (url, rx)
}

#[test]
fn a_real_run_posts_to_the_webhook_once_and_advances_the_cursor() {
    let fx = Fixture::new();
    configure(&fx);
    let (url, rx) = webhook_sink();
    let tier1 = fx.root().join("home/.config/autofactory/config.toml");
    let mut global = fs::read_to_string(&tier1).unwrap();
    global.push_str(&format!("\n[notifications]\nwebhook_url = \"{url}\"\n"));
    fs::write(&tier1, global).unwrap();
    let id = fx.add("make a file");
    assert_eq!(fx.aaf(&["notify", "--once"]).code, 0);

    assert_eq!(fx.aaf(&["run", "--unsandboxed"]).code, 0);
    emit(
        &fx,
        EventKind::TaskStateChanged,
        &id,
        json!({ "to": "blocked", "why": "already posted by the factory" }),
    );
    emit(&fx, EventKind::WebhookSent, &id, json!({ "ok": true }));

    let out = fx.aaf(&["notify", "--once"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["errors"], json!([]), "{}", out.stderr);
    let posted = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(posted["status"], json!("review"), "{posted}");
    assert_eq!(posted["task_id"], json!(id));
    // Blocked moments are posted by the factory itself, not a second time.
    assert!(rx.recv_timeout(Duration::from_millis(500)).is_err());

    let again = fx.aaf(&["notify", "--once"]);
    assert_eq!(again.json["alerts"], json!([]), "{}", again.stderr);
}

#[test]
fn all_scope_from_outside_any_repository_reports_a_real_block() {
    // As launchd / systemd start it: `--all`, working directory not a repo.
    let fx = Fixture::new();
    configure_with_test(&fx, "");
    let id = fx.add("make a file");
    let outside = fx.root();
    let notify =
        |fx: &Fixture| fx.aaf_in(&outside, &["notify", "--all", "--once", "--dry-run"], &[]);
    let first = notify(&fx);
    assert_eq!(first.code, 0, "{}", first.stderr);
    assert_eq!(first.json["alerts"], json!([]), "{}", first.stderr);
    seed_cursor(&fx, None, "all");

    let run = fx.aaf(&["run", "--unsandboxed"]);
    assert_eq!(run.json["blocked"], json!([id]), "{}", run.stderr);

    let out = notify(&fx);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(
        kinds(&out.json),
        [format!("blocked:\"{id}\"")],
        "{}",
        out.stderr
    );
    let text = out.json["alerts"][0]["text"].as_str().unwrap();
    assert!(text.contains("could not verify"), "{text}");
}
