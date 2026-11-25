#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Security and PWA tests for `trusted_origin` (Tailscale Serve / HTTPS reverse proxy)
//! and static PWA endpoints (`/manifest.webmanifest`, `/apple-touch-icon.png`, `/icon.svg`).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use super::{UiBackend, UiConfig, parse_trusted_origin, render_remote_pairing_card, start_server};

const TEST_TOKEN: &str = "abcdef0123456789abcdef0123456789";

struct DummyBackend;
impl UiBackend for DummyBackend {
    fn status_json(&self) -> Result<Value, String> {
        Ok(json!({ "ok": true }))
    }
    fn board_json(&self, _state: Option<&str>) -> Result<Value, String> {
        Ok(json!({ "tasks": [] }))
    }
    fn task_detail_json(&self, _id: &str) -> Result<Value, String> {
        Ok(json!({}))
    }
    fn handover_json(&self, _id: &str) -> Result<Value, String> {
        Ok(json!({}))
    }
    fn approve_preview_json(&self, _id: &str) -> Result<Value, String> {
        Ok(json!({}))
    }
    fn approve_commit(&self, _id: &str, _fp: &str, _rev: &str) -> Result<Value, String> {
        Ok(json!({}))
    }
    fn mutate_task(&self, _action: &str, _id: &str, _note: &str) -> Result<Value, String> {
        Ok(json!({ "ok": true }))
    }
    fn stream_tail_lines(&self, _run_id: &str, _max: usize) -> Result<Vec<String>, String> {
        Ok(vec![])
    }
    fn meta_json(&self) -> Result<Value, String> {
        Ok(json!({ "scope": "all" }))
    }
}

fn send_raw_bytes(addr: SocketAddr, req: &str) -> (u16, String, Vec<u8>) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    s.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
    s.write_all(req.as_bytes()).unwrap();
    s.flush().unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let sep = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..sep]).into_owned();
    let body = raw[sep + 4..].to_vec();
    let status: u16 = head
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    (status, head, body)
}

#[test]
fn parse_trusted_origin_enforces_https_and_rejects_malformed_inputs() {
    let ok = parse_trusted_origin("https://mac.tail1234.ts.net/").unwrap();
    assert_eq!(ok.host, "mac.tail1234.ts.net");
    assert_eq!(ok.port, 443);
    assert_eq!(ok.canonical, "https://mac.tail1234.ts.net");

    let custom = parse_trusted_origin("https://cockpit.example.com:8443").unwrap();
    assert_eq!(custom.port, 8443);
    assert_eq!(custom.canonical, "https://cockpit.example.com:8443");

    for bad in [
        "http://mac.tail1234.ts.net",
        "https://mac.tail1234.ts.net/path",
        "https://mac.tail1234.ts.net?x=1",
        "https://mac.tail1234.ts.net#token=abc",
        "https://user@mac.tail1234.ts.net",
        "https://*.ts.net",
        "https://0.0.0.0",
        "https://mac.tail1234.ts.net:0",
        "",
    ] {
        assert!(parse_trusted_origin(bad).is_err(), "should reject {bad:?}");
    }
}

#[test]
fn trusted_origin_allows_https_proxy_but_keeps_bearer_auth_and_blocks_foreign() {
    let origin = parse_trusted_origin("https://mac.tail1234.ts.net").unwrap();
    let cfg = UiConfig::resolve("127.0.0.1", 0, Some(TEST_TOKEN.into()), true)
        .unwrap()
        .with_trusted_origin(Some(origin));
    let handle = start_server(cfg, Arc::new(DummyBackend)).unwrap();
    let addr = handle.addr();

    // 1. Valid Host + Origin via Tailscale Serve (HTTPS 443) + Bearer token -> 200
    let (st_ok, _, body_ok) = send_raw_bytes(
        addr,
        &format!(
            "GET /api/meta HTTP/1.1\r\nHost: mac.tail1234.ts.net\r\n\
             Origin: https://mac.tail1234.ts.net\r\n\
             Authorization: Bearer {TEST_TOKEN}\r\n\r\n"
        ),
    );
    assert_eq!(st_ok, 200, "{}", String::from_utf8_lossy(&body_ok));

    // 2. Missing Bearer token MUST stay 401 even when Tailscale-User-Login header is present
    let (st_unauth, _, _) = send_raw_bytes(
        addr,
        "GET /api/meta HTTP/1.1\r\nHost: mac.tail1234.ts.net\r\n\
         Tailscale-User-Login: alex@example.com\r\n\r\n",
    );
    assert_eq!(st_unauth, 401);

    // 3. Downgraded http:// Origin on trusted host MUST be rejected 403
    let (st_http, _, _) = send_raw_bytes(
        addr,
        &format!(
            "POST /api/action HTTP/1.1\r\nHost: mac.tail1234.ts.net\r\n\
             Origin: http://mac.tail1234.ts.net\r\n\
             Authorization: Bearer {TEST_TOKEN}\r\nContent-Length: 2\r\n\r\n{{}}"
        ),
    );
    assert_eq!(st_http, 403);

    // 4. Foreign Host / Origin MUST be rejected 403
    let (st_foreign, _, _) = send_raw_bytes(
        addr,
        &format!(
            "GET /api/meta HTTP/1.1\r\nHost: evil.example.com\r\n\
             Authorization: Bearer {TEST_TOKEN}\r\n\r\n"
        ),
    );
    assert_eq!(st_foreign, 403);

    let (st_csrf, _, _) = send_raw_bytes(
        addr,
        &format!(
            "POST /api/action HTTP/1.1\r\nHost: mac.tail1234.ts.net\r\n\
             Origin: https://evil.example.com\r\n\
             Authorization: Bearer {TEST_TOKEN}\r\nContent-Length: 2\r\n\r\n{{}}"
        ),
    );
    assert_eq!(st_csrf, 403);

    // 5. Local loopback access still works alongside trusted_origin
    let (st_local, _, _) = send_raw_bytes(
        addr,
        &format!(
            "GET /api/meta HTTP/1.1\r\nHost: {addr}\r\n\
             Authorization: Bearer {TEST_TOKEN}\r\n\r\n"
        ),
    );
    assert_eq!(st_local, 200);

    handle.join();
}

#[test]
fn pwa_manifest_and_icons_are_served_without_auth_and_valid_png_magic() {
    let cfg = UiConfig::resolve("127.0.0.1", 0, Some(TEST_TOKEN.into()), false).unwrap();
    let handle = start_server(cfg, Arc::new(DummyBackend)).unwrap();
    let addr = handle.addr();

    let (st_m, head_m, body_m) = send_raw_bytes(
        addr,
        &format!("GET /manifest.webmanifest HTTP/1.1\r\nHost: {addr}\r\n\r\n"),
    );
    assert_eq!(st_m, 200);
    assert!(head_m.contains("application/manifest+json"));
    let manifest: Value = serde_json::from_slice(&body_m).unwrap();
    assert_eq!(manifest["display"], json!("standalone"));
    assert_eq!(manifest["start_url"], json!("/"));

    let (st_png, head_png, body_png) = send_raw_bytes(
        addr,
        &format!("GET /apple-touch-icon.png HTTP/1.1\r\nHost: {addr}\r\n\r\n"),
    );
    assert_eq!(st_png, 200);
    assert!(head_png.contains("image/png"));
    assert!(body_png.starts_with(b"\x89PNG\r\n\x1a\n"));

    let (st_svg, head_svg, body_svg) = send_raw_bytes(
        addr,
        &format!("GET /icon.svg HTTP/1.1\r\nHost: {addr}\r\n\r\n"),
    );
    assert_eq!(st_svg, 200);
    assert!(head_svg.contains("image/svg+xml"));
    assert!(String::from_utf8_lossy(&body_svg).contains("<svg"));

    handle.join();
}

#[test]
fn remote_pairing_card_falls_back_to_clean_url_qr_when_full_url_exceeds_106_bytes() {
    let short = parse_trusted_origin("https://mac.tail1.ts.net").unwrap();
    let card_short = render_remote_pairing_card(&short, TEST_TOKEN, false, true);
    assert!(card_short.contains("█"));
    assert!(!card_short.contains("Full URL > 106 bytes"));

    let long = parse_trusted_origin(
        "https://very-long-macbook-pro-workstation-hostname.tailscale1234567890.ts.net",
    )
    .unwrap();
    assert!(long.pairing_url(TEST_TOKEN).len() > 106);
    assert!(long.clean_url().len() <= 106);
    let card_long = render_remote_pairing_card(&long, TEST_TOKEN, false, true);
    assert!(card_long.contains("Full URL > 106 bytes"));
    assert!(card_long.contains("█"), "must fall back to clean-URL QR");
}

#[test]
fn trusted_origin_custom_port_and_ipv6_validation() {
    let default_port = parse_trusted_origin("https://mac.tail1234.ts.net").unwrap();
    assert!(default_port.matches_host("mac.tail1234.ts.net", None));
    assert!(default_port.matches_host("mac.tail1234.ts.net", Some(443)));
    assert!(!default_port.matches_host("mac.tail1234.ts.net", Some(8443)));

    let custom = parse_trusted_origin("https://cockpit.example.com:8443").unwrap();
    assert!(
        !custom.matches_host("cockpit.example.com", None),
        "portless Host must default to 443 and not bypass custom port :8443"
    );
    assert!(!custom.matches_host("cockpit.example.com", Some(443)));
    assert!(custom.matches_host("cockpit.example.com", Some(8443)));
    assert_eq!(custom.origin_header(), "https://cockpit.example.com:8443");

    let v6 = parse_trusted_origin("https://[2001:db8::1]:8443").unwrap();
    assert_eq!(v6.canonical, "https://[2001:db8::1]:8443");
    assert!(v6.matches_host("2001:db8::1", Some(8443)));
    assert!(!v6.matches_host("2001:db8::1", None));

    for bad in [
        "https://0.0.0.0",
        "https://0.0.0.0:8443",
        "https://[::]",
        "https://[::]:8443",
        "https://[0:0:0:0:0:0:0:0]",
        "https://::1",
        "https://2001:db8::1",
        "https://[not-ipv6]",
        "https://foo:bar:baz",
    ] {
        assert!(parse_trusted_origin(bad).is_err(), "should reject {bad:?}");
    }
}

#[test]
fn trusted_origin_host_rejects_http_origin_fallback_and_hardens_csp() {
    assert!(super::HTML_CSP.contains("base-uri 'none'"));
    assert!(super::HTML_CSP.contains("form-action 'self'"));
    assert!(super::HTML_CSP.contains("object-src 'none'"));

    let origin = parse_trusted_origin("https://mac.tail1234.ts.net").unwrap();
    let cfg = UiConfig::resolve("127.0.0.1", 0, Some(TEST_TOKEN.into()), true)
        .unwrap()
        .with_trusted_origin(Some(origin));
    let handle = start_server(cfg, Arc::new(DummyBackend)).unwrap();
    let addr = handle.addr();
    let port = addr.port();

    // Even when Origin matches loopback + bound_port, Host matched ONLY via trusted_origin
    // MUST require exact https:// trusted_origin.origin_header() and reject http:// fallback.
    let (st_loopback_origin, _, _) = send_raw_bytes(
        addr,
        &format!(
            "POST /api/action HTTP/1.1\r\nHost: mac.tail1234.ts.net\r\n\
             Origin: http://127.0.0.1:{port}\r\n\
             Authorization: Bearer {TEST_TOKEN}\r\nContent-Length: 2\r\n\r\n{{}}"
        ),
    );
    assert_eq!(st_loopback_origin, 403);

    // Same-port trusted_origin with http:// Origin MUST also be rejected in LAN mode.
    let same_port_origin =
        parse_trusted_origin(&format!("https://mac.tail1234.ts.net:{port}")).unwrap();
    let lan_cfg = UiConfig {
        host: "0.0.0.0".to_string(),
        port,
        token: TEST_TOKEN.to_string(),
        allow_mutating: true,
        allow_approve: false,
        lan_mode: true,
        trusted_origin: Some(same_port_origin),
    };
    let req = super::RequestSecurityContext {
        method: "POST",
        path: "/api/action",
        host_header: Some(&format!("mac.tail1234.ts.net:{port}")),
        origin_header: Some(&format!("http://mac.tail1234.ts.net:{port}")),
        auth_header: Some(&format!("Bearer {TEST_TOKEN}")),
        query_token: None,
        require_auth: true,
        allow_query_token: false,
        is_mutating: true,
    };
    assert!(matches!(
        super::verify_request_security(&lan_cfg, port, &req),
        Err(super::SecurityRejection::Forbidden(_))
    ));

    handle.join();
}
