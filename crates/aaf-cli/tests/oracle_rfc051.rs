#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Oracle test for RFC-051: `aaf ui --allow-approve` opt-in gate for `POST /api/approve`.

#[path = "cli_support.rs"]
mod support;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aaf_engine::web_ui::{UiBackend, UiConfig, start_server};
use serde_json::{Value, json};
use support::Sandbox;

const TEST_TOKEN: &str = "0123456789abcdef0123456789abcdef";

#[derive(Default)]
struct RecordingBackend {
    approved: Mutex<Vec<(String, String, String)>>,
    actions: Mutex<Vec<(String, String, String)>>,
}

impl UiBackend for RecordingBackend {
    fn status_json(&self) -> Result<Value, String> {
        Ok(json!({ "scope": "/tmp/rfc051", "frozen": false }))
    }

    fn board_json(&self, _state: Option<&str>) -> Result<Value, String> {
        Ok(json!({ "tasks": [] }))
    }

    fn task_detail_json(&self, task_id: &str) -> Result<Value, String> {
        Ok(json!({ "id": task_id }))
    }

    fn handover_json(&self, task_id: &str) -> Result<Value, String> {
        Ok(json!({ "task_id": task_id }))
    }

    fn approve_preview_json(&self, task_id: &str) -> Result<Value, String> {
        Ok(json!({
            "task_id": task_id,
            "lock_fp": "fp-locked-051",
            "locked_changes": [{ "change": "M", "path": "tests/it.rs" }],
        }))
    }

    fn approve_commit(
        &self,
        task_id: &str,
        expected_lock_fp: &str,
        reviewer: &str,
    ) -> Result<Value, String> {
        self.approved.lock().unwrap().push((
            task_id.to_string(),
            expected_lock_fp.to_string(),
            reviewer.to_string(),
        ));
        Ok(json!({
            "approved": true,
            "task_id": task_id,
            "lock_fp": expected_lock_fp,
            "reviewer": reviewer,
        }))
    }

    fn mutate_task(&self, action: &str, task_id: &str, note: &str) -> Result<Value, String> {
        self.actions.lock().unwrap().push((
            action.to_string(),
            task_id.to_string(),
            note.to_string(),
        ));
        Ok(json!({ "action": action, "task_id": task_id, "note": note }))
    }

    fn stream_tail_lines(&self, _run_id: &str, _max_lines: usize) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }

    fn meta_json(&self) -> Result<Value, String> {
        Ok(json!({ "repo_name": "rfc051", "read_only": false, "version": "test" }))
    }
}

struct RawResp {
    status: u16,
    _headers: HashMap<String, String>,
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
        _headers: headers,
        body: body.to_string(),
    }
}

fn get_authed(addr: SocketAddr, path: &str) -> RawResp {
    let req = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {addr}\r\n\
         Origin: http://{addr}\r\n\
         Authorization: Bearer {TEST_TOKEN}\r\n\r\n"
    );
    send_raw(addr, &req)
}

fn post_approve(addr: SocketAddr) -> RawResp {
    let payload = r#"{"task_id":"t-51","lock_fp":"fp-locked-051","reviewer":"alice"}"#;
    let req = format!(
        "POST /api/approve HTTP/1.1\r\n\
         Host: {addr}\r\n\
         Origin: http://{addr}\r\n\
         Authorization: Bearer {TEST_TOKEN}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{payload}",
        payload.len()
    );
    send_raw(addr, &req)
}

#[test]
fn default_server_blocks_post_approve_with_403_and_does_not_sign() {
    assert!(
        !UiConfig::default().allow_approve,
        "UiConfig::default().allow_approve must be false"
    );
    let backend = Arc::new(RecordingBackend::default());
    let cfg = UiConfig::resolve("127.0.0.1", 0, Some(TEST_TOKEN.to_string()), true).unwrap();
    assert!(!cfg.allow_approve);
    let handle = start_server(cfg, backend.clone()).unwrap();
    let addr = handle.addr();

    // (a) Default server rejects POST /api/approve with 403 and never invokes callback.
    let resp = post_approve(addr);
    assert_eq!(
        resp.status, 403,
        "default Web UI server must reject POST /api/approve with 403: {}",
        resp.body
    );
    let body: Value = serde_json::from_str(&resp.body).unwrap();
    assert_eq!(body["ok"], json!(false));
    let err = body["error"].as_str().unwrap_or("");
    assert!(
        err.contains("--allow-approve") && err.contains("aaf approve"),
        "403 error should explain --allow-approve and aaf approve, got: {err}"
    );
    assert!(
        backend.approved.lock().unwrap().is_empty(),
        "approve_commit callback must NOT be called when allow_approve is false"
    );

    // /api/meta reflects allow_approve = false, and other endpoints remain unaffected.
    let meta_resp = get_authed(addr, "/api/meta");
    assert_eq!(meta_resp.status, 200);
    let meta: Value = serde_json::from_str(&meta_resp.body).unwrap();
    assert_eq!(meta["allow_approve"], json!(false));
    assert_eq!(meta["read_only"], json!(false));

    let health = send_raw(
        addr,
        &format!("GET /api/health HTTP/1.1\r\nHost: {addr}\r\n\r\n"),
    );
    assert_eq!(health.status, 200);
    assert_eq!(
        serde_json::from_str::<Value>(&health.body).unwrap(),
        json!({ "ok": true })
    );

    assert_eq!(get_authed(addr, "/api/board").status, 200);
    assert_eq!(get_authed(addr, "/api/approve/preview/t-51").status, 200);

    // Non-approve mutations still work when not read-only.
    let action_body = r#"{"action":"steer","task_id":"t-51","note":"keep going"}"#;
    let action_resp = send_raw(
        addr,
        &format!(
            "POST /api/action HTTP/1.1\r\nHost: {addr}\r\nOrigin: http://{addr}\r\n\
             Authorization: Bearer {TEST_TOKEN}\r\nContent-Length: {}\r\n\r\n{action_body}",
            action_body.len()
        ),
    );
    assert_eq!(action_resp.status, 200);
    assert_eq!(backend.actions.lock().unwrap().len(), 1);

    // Embedded index.html disables approve and shows escaped hint when !allow_approve.
    let idx = send_raw(addr, &format!("GET / HTTP/1.1\r\nHost: {addr}\r\n\r\n"));
    assert_eq!(idx.status, 200);
    assert!(idx.body.contains("meta.allow_approve"));
    assert!(idx.body.contains("--allow-approve"));
    assert!(idx.body.contains("esc(approveHint)"));

    handle.join();
}

#[test]
fn opt_in_allow_approve_permits_post_approve_when_not_read_only() {
    let backend = Arc::new(RecordingBackend::default());
    let cfg = UiConfig::resolve("127.0.0.1", 0, Some(TEST_TOKEN.to_string()), true)
        .unwrap()
        .with_allow_approve(true);
    assert!(cfg.allow_approve);
    assert!(cfg.effective_allow_approve());
    let handle = start_server(cfg, backend.clone()).unwrap();
    let addr = handle.addr();

    let meta: Value = serde_json::from_str(&get_authed(addr, "/api/meta").body).unwrap();
    assert_eq!(meta["allow_approve"], json!(true));

    // (b) Opt-in allow_approve=true allows POST /api/approve to reach handler.
    let resp = post_approve(addr);
    assert_eq!(resp.status, 200, "unexpected response: {}", resp.body);
    let body: Value = serde_json::from_str(&resp.body).unwrap();
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["approved"], json!(true));
    assert_eq!(
        *backend.approved.lock().unwrap(),
        vec![(
            "t-51".to_string(),
            "fp-locked-051".to_string(),
            "alice".to_string()
        )]
    );

    handle.join();
}

#[test]
fn read_only_forces_403_even_when_allow_approve_is_true() {
    // (c1) Builder helper forces fail-closed.
    let resolved = UiConfig::resolve("127.0.0.1", 0, Some(TEST_TOKEN.to_string()), false)
        .unwrap()
        .with_allow_approve(true);
    assert!(!resolved.allow_approve);
    assert!(!resolved.effective_allow_approve());

    // (c2) Even if a caller manually sets both allow_mutating=false and allow_approve=true,
    // the server must still fail-closed with 403 and never call approve_commit.
    let backend = Arc::new(RecordingBackend::default());
    let manual = UiConfig {
        host: "127.0.0.1".to_string(),
        port: 0,
        token: TEST_TOKEN.to_string(),
        allow_mutating: false,
        allow_approve: true,
        lan_mode: false,
        trusted_origin: None,
    };
    assert!(!manual.effective_allow_approve());
    let handle = start_server(manual, backend.clone()).unwrap();
    let addr = handle.addr();

    let resp = post_approve(addr);
    assert_eq!(
        resp.status, 403,
        "read_only must block approve: {}",
        resp.body
    );
    assert!(backend.approved.lock().unwrap().is_empty());

    let meta: Value = serde_json::from_str(&get_authed(addr, "/api/meta").body).unwrap();
    assert_eq!(meta["read_only"], json!(true));
    assert_eq!(meta["allow_approve"], json!(false));

    handle.join();
}

#[test]
fn cli_ui_help_lists_allow_approve_and_warns_about_key_argv() {
    let sb = Sandbox::new();
    let out = sb.aaf(&["ui", "--help"]);
    assert!(out.status.success(), "aaf ui --help failed");
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        help.contains("--allow-approve"),
        "aaf ui --help must list --allow-approve:\n{help}"
    );
    assert!(
        help.contains("AAF_UI_TOKEN"),
        "aaf ui --help must recommend AAF_UI_TOKEN:\n{help}"
    );
    assert!(
        help.contains("argv") || help.contains("ps"),
        "aaf ui --help must warn about --key exposing token in argv/ps:\n{help}"
    );

    // Binary parses `--allow-approve` cleanly (reaches AAF_RUN_ID policy guard, not clap usage error).
    let agent = sb
        .command(&["ui", "--allow-approve", "--json"])
        .env("AAF_RUN_ID", "run-51")
        .output()
        .unwrap();
    assert_eq!(agent.status.code(), Some(2));
    let parsed: Value = serde_json::from_slice(&agent.stdout).unwrap();
    assert_eq!(parsed["command"], json!("ui"));
    assert_eq!(parsed["status"], json!("refused"));
}
