//! `/studio` page and the `/api/report` and `/api/doctor` routes behind it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::tests::{TEST_TOKEN, send_raw};
use super::{HTML_CSP, UiBackend, UiConfig, start_server};

#[derive(Default)]
struct StudioBackend {
    since_seen: Mutex<Vec<Option<u64>>>,
}

impl UiBackend for StudioBackend {
    fn status_json(&self) -> Result<Value, String> {
        Ok(json!({}))
    }
    fn board_json(&self, _: Option<&str>) -> Result<Value, String> {
        Ok(json!({ "tasks": [] }))
    }
    fn task_detail_json(&self, _: &str) -> Result<Value, String> {
        Err("not found: x".into())
    }
    fn handover_json(&self, _: &str) -> Result<Value, String> {
        Err("not found: x".into())
    }
    fn approve_preview_json(&self, _: &str) -> Result<Value, String> {
        Err("not found: x".into())
    }
    fn approve_commit(&self, _: &str, _: &str, _: &str) -> Result<Value, String> {
        Err("not found: x".into())
    }
    fn mutate_task(&self, _: &str, _: &str, _: &str) -> Result<Value, String> {
        Err("not found: x".into())
    }
    fn stream_tail_lines(&self, _: &str, _: usize) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }
    fn meta_json(&self) -> Result<Value, String> {
        Ok(json!({ "repo_name": "r" }))
    }
    fn report_json(&self, since_hours: Option<u64>) -> Result<Value, String> {
        self.since_seen.lock().unwrap().push(since_hours);
        Ok(json!({ "ok": true, "report": { "tasks_done": 4 } }))
    }
    fn doctor_json(&self) -> Result<Value, String> {
        Ok(json!({ "ok": true, "checks": [] }))
    }
}

fn start() -> (super::UiServerHandle, Arc<StudioBackend>) {
    let backend = Arc::new(StudioBackend::default());
    let cfg = UiConfig {
        host: "127.0.0.1".to_string(),
        port: 0,
        token: TEST_TOKEN.to_string(),
        allow_mutating: false,
        allow_approve: false,
        lan_mode: false,
        trusted_origin: None,
    };
    (start_server(cfg, backend.clone()).unwrap(), backend)
}

fn get(addr: std::net::SocketAddr, path: &str, auth: bool) -> super::tests::RawResp {
    let auth = if auth {
        format!("Authorization: Bearer {TEST_TOKEN}\r\n")
    } else {
        String::new()
    };
    send_raw(
        addr,
        &format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\n{auth}\r\n"),
    )
}

#[test]
fn studio_page_is_served_with_the_strict_csp_and_no_data() {
    let (handle, _) = start();
    for path in ["/health", "/studio"] {
        let resp = get(handle.addr(), path, false);
        assert_eq!(resp.status, 200, "route {path}");
        assert!(resp.body.contains("<title>AAF Health</title>"));
        assert!(resp.body.contains("/api/report"));
        assert!(resp.body.contains("id=\"tokenDiagCard\""));
        assert!(resp.body.contains("id=\"opsHealthCard\""));
        assert!(resp.body.contains("/api/streams/"));
        assert!(resp.body.contains("wasted_desc"));
        assert!(!resp.body.contains(TEST_TOKEN));
        assert_eq!(
            resp.headers
                .get("content-security-policy")
                .map(String::as_str),
            Some(HTML_CSP)
        );
        // No external resource: the CSP would block it anyway.
        assert!(!resp.body.contains("https://"));
    }
    handle.join();
}

#[test]
fn report_and_doctor_need_the_bearer_token() {
    let (handle, _) = start();
    let addr = handle.addr();
    assert_eq!(get(addr, "/api/report", false).status, 401);
    assert_eq!(get(addr, "/api/doctor", false).status, 401);
    let q = format!("/api/report?token={TEST_TOKEN}");
    assert_eq!(get(addr, &q, false).status, 401, "query token refused");
    handle.join();
}

#[test]
fn report_passes_the_period_and_doctor_answers() {
    let (handle, backend) = start();
    let addr = handle.addr();
    let resp = get(addr, "/api/report?since=24", true);
    assert_eq!(resp.status, 200);
    let body: Value = serde_json::from_str(&resp.body).unwrap();
    assert_eq!(body["report"]["tasks_done"], 4);
    assert_eq!(get(addr, "/api/report", true).status, 200);
    assert_eq!(get(addr, "/api/report?hours=48", true).status, 200);
    assert_eq!(get(addr, "/api/report?since=0", true).status, 200);
    assert_eq!(get(addr, "/api/report?since=abc", true).status, 200);
    assert_eq!(
        *backend.since_seen.lock().unwrap(),
        vec![Some(24), None, Some(48), None, None]
    );
    assert_eq!(get(addr, "/api/doctor", true).status, 200);
    handle.join();
}

#[test]
fn studio_routes_are_get_only() {
    let (handle, _) = start();
    let addr = handle.addr();
    for path in ["/api/report", "/api/doctor"] {
        let resp = send_raw(
            addr,
            &format!(
                "POST {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {TEST_TOKEN}\r\nContent-Length: 0\r\n\r\n"
            ),
        );
        assert_eq!(resp.status, 405, "{path} must reject POST");
    }
    handle.join();
}
