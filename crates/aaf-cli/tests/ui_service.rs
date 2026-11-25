#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]
//! Integration tests for `aaf ui --service` (always-on, strictly read-only loopback Cockpit).

#[path = "cli_support.rs"]
mod support;

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::Sandbox;

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_loopback_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.local_addr().unwrap().port()
}

fn raw_http(port: u16, req: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream.write_all(req.as_bytes()).unwrap();
    stream.flush().unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8(raw).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    let status: u16 = head
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    (status, body.to_string())
}

fn wait_for_health(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(mut s) = TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            Duration::from_millis(100),
        ) {
            let _ = s.set_read_timeout(Some(Duration::from_millis(300)));
            let req = format!("GET /api/health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n");
            if s.write_all(req.as_bytes()).is_ok() {
                let mut buf = Vec::new();
                if s.read_to_end(&mut buf).is_ok()
                    && String::from_utf8_lossy(&buf).starts_with("HTTP/1.1 200")
                {
                    return;
                }
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("aaf ui --service did not become healthy on port {port}");
}

#[test]
fn service_url_creates_persistent_0600_token_and_reuses_it() {
    let sb = Sandbox::new();
    let (code1, out1) = sb.json(&["ui", "--service-url"]);
    assert_eq!(code1, 0, "{out1}");
    assert_eq!(out1["ok"], json!(true));
    assert_eq!(out1["read_only"], json!(true));
    let url1 = out1["url"].as_str().unwrap();
    assert!(url1.starts_with("http://127.0.0.1:4317/#token="), "{url1}");
    let studio1 = out1["studio_url"].as_str().unwrap();
    assert!(
        studio1.starts_with("http://127.0.0.1:4317/studio#token="),
        "{studio1}"
    );

    let tok_path = sb.data.join("ui-service.token");
    assert!(tok_path.is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&tok_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    let (code2, out2) = sb.json(&["ui", "--service-url"]);
    assert_eq!(code2, 0, "{out2}");
    assert_eq!(out2["url"], out1["url"], "token must stay stable");
}

#[test]
fn service_refuses_non_loopback_allow_approve_and_agent_run() {
    let sb = Sandbox::new();

    // Non-loopback host is refused in --service mode.
    let (code, out) = sb.json(&["ui", "--service", "--host", "0.0.0.0"]);
    assert_eq!(code, 2, "{out}");
    assert_eq!(out["ok"], json!(false));
    assert_eq!(out["status"], json!("refused"));

    // --allow-approve conflicts with --service and --allow-actions.
    let conflict = sb.aaf(&["ui", "--service", "--allow-approve"]);
    assert_eq!(conflict.status.code(), Some(2));
    let conflict2 = sb.aaf(&["ui", "--service", "--allow-actions", "--allow-approve"]);
    assert_eq!(conflict2.status.code(), Some(2));

    // Inside agent run (AAF_RUN_ID set), service flags are refused.
    for args in [
        &["ui", "--service-url", "--json"][..],
        &["ui", "--service", "--json"][..],
        &["ui", "--install-service", "--json"][..],
    ] {
        let out = sb
            .command(args)
            .env("AAF_RUN_ID", "run-agent-1")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["status"], json!("refused"));
    }
}

#[test]
fn service_mode_serves_cockpit_and_studio_strictly_read_only() {
    let sb = Sandbox::new();
    let port = free_loopback_port();
    let child = sb
        .command(&["ui", "--service", "--port", &port.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _guard = ChildGuard(child);

    wait_for_health(port);

    let tok_path = sb.data.join("ui-service.token");
    let token = fs::read_to_string(&tok_path).unwrap().trim().to_string();
    assert_eq!(token.len(), 32);

    // HTML pages render without auth (no secrets embedded).
    let (st_idx, idx_body) = raw_http(
        port,
        &format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
    );
    assert_eq!(st_idx, 200);
    assert!(idx_body.contains("AutoAgentFactory Dashboard"));
    assert!(idx_body.contains("Kanban"));
    assert!(!idx_body.contains(&token));

    let (st_studio, studio_body) = raw_http(
        port,
        &format!("GET /studio HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
    );
    assert_eq!(st_studio, 200);
    assert!(studio_body.contains("<title>AAF Studio</title>"));
    assert!(!studio_body.contains(&token));

    // Unauthenticated API calls are rejected.
    let (st_unauth, _) = raw_http(
        port,
        &format!("GET /api/meta HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
    );
    assert_eq!(st_unauth, 401);

    // Authenticated /api/meta reports read_only=true, allow_approve=false, scope=all.
    let (st_meta, meta_body) = raw_http(
        port,
        &format!(
            "GET /api/meta HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\n\r\n"
        ),
    );
    assert_eq!(st_meta, 200, "{meta_body}");
    let meta: Value = serde_json::from_str(&meta_body).unwrap();
    assert_eq!(meta["read_only"], json!(true));
    assert_eq!(meta["allow_approve"], json!(false));
    assert_eq!(meta["scope"], json!("all"));

    // Every mutating endpoint is strictly forbidden (403) even with a valid token.
    let action_payload = r#"{"action":"unfreeze","task_id":"","note":""}"#;
    let (st_act, act_body) = raw_http(
        port,
        &format!(
            "POST /api/action HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{action_payload}",
            action_payload.len()
        ),
    );
    assert_eq!(st_act, 403, "{act_body}");

    let approve_payload = r#"{"task_id":"t-1","lock_fp":"abc","reviewer":"me"}"#;
    let (st_app, app_body) = raw_http(
        port,
        &format!(
            "POST /api/approve HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{approve_payload}",
            approve_payload.len()
        ),
    );
    assert_eq!(st_app, 403, "{app_body}");
}

#[test]
fn service_allow_actions_enables_ready_and_cancel_but_keeps_approve_locked() {
    use aaf_domain::ports::TaskStore;
    use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
    use aaf_engine::db::Db;

    let sb = Sandbox::new();
    fs::create_dir_all(&sb.data).unwrap();
    let db = Db::open(&sb.data.join("aaf.db")).unwrap();
    let mut task = Task::new_root(
        "t-backlog-ui",
        sb.repo.to_string_lossy(),
        "Backlog UI task",
        ImmutableSpec {
            description: "Test promoting from UI".into(),
            acceptance: vec!["moves to ready".into()],
        },
        Priority::P2,
        1_700_000_000,
    );
    task.state = TaskState::Backlog;
    db.insert_task(&task).unwrap();
    drop(db);
    let task_id = "t-backlog-ui".to_string();

    let port = free_loopback_port();
    let child = sb
        .command(&[
            "ui",
            "--service",
            "--allow-actions",
            "--port",
            &port.to_string(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _guard = ChildGuard(child);
    wait_for_health(port);

    let tok_path = sb.data.join("ui-service.token");
    let token = fs::read_to_string(&tok_path).unwrap().trim().to_string();

    let (st_meta, meta_body) = raw_http(
        port,
        &format!(
            "GET /api/meta HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\n\r\n"
        ),
    );
    assert_eq!(st_meta, 200, "{meta_body}");
    let meta: Value = serde_json::from_str(&meta_body).unwrap();
    assert_eq!(meta["read_only"], json!(false));
    assert_eq!(meta["allow_approve"], json!(false));

    // Promote backlog -> ready
    let ready_payload = json!({ "action": "ready", "task_id": task_id, "note": "" }).to_string();
    let (st_ready, ready_body) = raw_http(
        port,
        &format!(
            "POST /api/action HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{ready_payload}",
            ready_payload.len()
        ),
    );
    assert_eq!(st_ready, 200, "{ready_body}");
    let ready_val: Value = serde_json::from_str(&ready_body).unwrap();
    assert_eq!(ready_val["state"], json!("ready"));

    // Cancel task
    let cancel_payload =
        json!({ "action": "cancel", "task_id": task_id, "note": "not needed" }).to_string();
    let (st_cancel, cancel_body) = raw_http(
        port,
        &format!(
            "POST /api/action HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{cancel_payload}",
            cancel_payload.len()
        ),
    );
    assert_eq!(st_cancel, 200, "{cancel_body}");
    let cancel_val: Value = serde_json::from_str(&cancel_body).unwrap();
    assert_eq!(cancel_val["state"], json!("cancelled"));

    // Approve remains strictly 403 Forbidden even with --allow-actions!
    let approve_payload = r#"{"task_id":"t-1","lock_fp":"abc","reviewer":"me"}"#;
    let (st_app, app_body) = raw_http(
        port,
        &format!(
            "POST /api/approve HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{approve_payload}",
            approve_payload.len()
        ),
    );
    assert_eq!(st_app, 403, "{app_body}");
}

#[test]
fn service_new_endpoints_create_task_diff_proof_and_supervisor() {
    let sb = Sandbox::new();
    let port = free_loopback_port();
    let child = sb
        .command(&[
            "ui",
            "--service",
            "--allow-actions",
            "--port",
            &port.to_string(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _guard = ChildGuard(child);
    wait_for_health(port);

    let tok_path = sb.data.join("ui-service.token");
    let token = fs::read_to_string(&tok_path).unwrap().trim().to_string();

    // 1. Create a Backlog task via POST /api/tasks
    let create_payload = json!({
        "repo_root": sb.repo.to_string_lossy(),
        "title": "UI created task",
        "spec": "Created from Cockpit modal",
        "accept": ["passes check"],
        "priority": "P1",
        "state": "backlog"
    })
    .to_string();
    let (st_create, create_body) = raw_http(
        port,
        &format!(
            "POST /api/tasks HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{create_payload}",
            create_payload.len()
        ),
    );
    assert_eq!(st_create, 200, "{create_body}");
    let created: Value = serde_json::from_str(&create_body).unwrap();
    let new_id = created["task_id"].as_str().unwrap().to_string();
    assert!(new_id.starts_with("t-"), "{created}");
    assert_eq!(created["state"], json!("backlog"));
    assert_eq!(created["priority"], json!("P1"));

    // 2. GET /api/diff/<id> & GET /api/proof/<id> require auth and return available=false cleanly
    let (st_diff_unauth, _) = raw_http(
        port,
        &format!("GET /api/diff/{new_id} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
    );
    assert_eq!(st_diff_unauth, 401);

    let (st_diff, diff_body) = raw_http(
        port,
        &format!(
            "GET /api/diff/{new_id} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\n\r\n"
        ),
    );
    assert_eq!(st_diff, 200, "{diff_body}");
    let diff_val: Value = serde_json::from_str(&diff_body).unwrap();
    assert_eq!(diff_val["available"], json!(false));

    let (st_proof, proof_body) = raw_http(
        port,
        &format!(
            "GET /api/proof/{new_id} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\n\r\n"
        ),
    );
    assert_eq!(st_proof, 200, "{proof_body}");
    let proof_val: Value = serde_json::from_str(&proof_body).unwrap();
    assert_eq!(proof_val["available"], json!(false));

    let post_action = |body: Value| -> Value {
        let s = body.to_string();
        let (st, resp) = raw_http(
            port,
            &format!(
                "POST /api/action HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{s}",
                s.len()
            ),
        );
        assert_eq!(st, 200, "{resp}");
        serde_json::from_str(&resp).unwrap()
    };
    let get_auth = |path: &str| -> Value {
        let (st, resp) = raw_http(
            port,
            &format!(
                "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\n\r\n"
            ),
        );
        assert_eq!(st, 200, "{resp}");
        serde_json::from_str(&resp).unwrap()
    };

    // 3. Supervisor toggle ON then OFF persists and reflects in /api/status
    let on_val = post_action(json!({ "action": "supervisor_toggle", "task_id": "", "note": "on" }));
    assert_eq!(on_val["enabled"], json!(true));
    assert_eq!(
        get_auth("/api/status")["supervisor"]["enabled"],
        json!(true)
    );
    let off_val =
        post_action(json!({ "action": "supervisor_toggle", "task_id": "", "note": "off" }));
    assert_eq!(off_val["enabled"], json!(false));

    // 3b. Supervisor allowed_repos selection persists and reflects in /api/status
    let repo_canon = std::fs::canonicalize(&sb.repo)
        .unwrap_or_else(|_| sb.repo.clone())
        .to_string_lossy()
        .to_string();
    let pick_val = post_action(json!({
        "action": "supervisor_repos",
        "task_id": "",
        "note": json!({ "repos": [sb.repo.to_string_lossy()] }).to_string(),
    }));
    assert_eq!(pick_val["allowed_repos"], json!([repo_canon]));
    let status_val2 = get_auth("/api/status");
    assert_eq!(
        status_val2["supervisor"]["allowed_repos"],
        json!([repo_canon])
    );
    assert!(status_val2["supervisor"]["known_repos"].is_array());
    let none_val = post_action(json!({
        "action": "supervisor_repos",
        "task_id": "",
        "note": json!({ "repos": [] }).to_string(),
    }));
    assert_eq!(none_val["allowed_repos"], json!([]));
    let all_val = post_action(json!({
        "action": "supervisor_repos",
        "task_id": "",
        "note": json!({ "all": true }).to_string(),
    }));
    assert!(all_val["allowed_repos"].is_null());

    // 4. GET /api/accounts requires auth & returns native repo_assignments + aaf_accounts
    let (st_acc_unauth, _) = raw_http(
        port,
        &format!("GET /api/accounts HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
    );
    assert_eq!(st_acc_unauth, 401);

    let (st_acc, acc_body) = raw_http(
        port,
        &format!(
            "GET /api/accounts HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\n\r\n"
        ),
    );
    assert_eq!(st_acc, 200, "{acc_body}");
    let acc_val: Value = serde_json::from_str(&acc_body).unwrap();
    assert!(acc_val["aaf_accounts"].is_array());
    assert!(acc_val["repo_assignments"].is_array());

    // 5. GET /api/report returns per-task breakdown including the newly created task
    let (st_rep, rep_body) = raw_http(
        port,
        &format!(
            "GET /api/report HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\n\r\n"
        ),
    );
    assert_eq!(st_rep, 200, "{rep_body}");
    let rep_val: Value = serde_json::from_str(&rep_body).unwrap();
    let tb = rep_val["task_breakdown"].as_array().unwrap();
    assert!(tb.iter().any(|item| item["id"] == json!(new_id)));

    // 6. Native Repo <-> Account assign & remove writes .claude/settings.local.json safely
    let assign_payload = json!({
        "action": "cc_repo_assign",
        "task_id": sb.repo.to_string_lossy(),
        "note": "finos_toan"
    })
    .to_string();
    let (st_asg, asg_body) = raw_http(
        port,
        &format!(
            "POST /api/action HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{assign_payload}",
            assign_payload.len()
        ),
    );
    assert_eq!(st_asg, 200, "{asg_body}");
    let local_settings: Value = serde_json::from_str(
        &fs::read_to_string(sb.repo.join(".claude/settings.local.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(local_settings["account"], json!("finos_toan"));
}

#[test]
fn service_read_only_blocks_create_task_and_supervisor_toggle() {
    let sb = Sandbox::new();
    let port = free_loopback_port();
    let child = sb
        .command(&["ui", "--service", "--port", &port.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _guard = ChildGuard(child);
    wait_for_health(port);

    let tok_path = sb.data.join("ui-service.token");
    let token = fs::read_to_string(&tok_path).unwrap().trim().to_string();

    let create_payload = json!({
        "repo_root": sb.repo.to_string_lossy(),
        "title": "Blocked task",
        "description": "Should fail in read-only mode",
        "priority": "P2",
        "state": "ready"
    })
    .to_string();
    let (st_create, create_body) = raw_http(
        port,
        &format!(
            "POST /api/tasks HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{create_payload}",
            create_payload.len()
        ),
    );
    assert_eq!(st_create, 403, "{create_body}");

    let sup_on = json!({ "action": "supervisor_toggle", "task_id": "on", "note": "" }).to_string();
    let (st_sup, sup_body) = raw_http(
        port,
        &format!(
            "POST /api/action HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{sup_on}",
            sup_on.len()
        ),
    );
    assert_eq!(st_sup, 403, "{sup_body}");
}

#[cfg(unix)]
#[test]
fn service_url_ignores_legacy_symlink_without_following_or_deleting_it() {
    let sb = Sandbox::new();
    let outside = sb.home.join("do-not-read.token");
    let secret = "fedcba9876543210fedcba9876543210";
    let config_dir = sb.home.join(".config/autofactory");
    fs::create_dir_all(&sb.home).unwrap();
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(&outside, secret).unwrap();
    let legacy = config_dir.join("ui-service.token");
    std::os::unix::fs::symlink(&outside, &legacy).unwrap();

    let (code, out) = sb.json(&["ui", "--service-url"]);
    assert_eq!(code, 0, "{out}");
    let url = out["url"].as_str().unwrap();
    assert!(
        !url.contains(secret),
        "must not adopt token from symlink: {url}"
    );
    assert!(
        fs::symlink_metadata(&legacy).is_ok_and(|m| m.file_type().is_symlink()),
        "legacy symlink must not be deleted"
    );
    assert_eq!(fs::read_to_string(&outside).unwrap(), secret);
}
