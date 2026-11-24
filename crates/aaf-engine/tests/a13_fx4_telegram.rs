#![allow(clippy::unwrap_used, clippy::expect_used)]
//! A13 L8-2: `getUpdates` bounds its batch size, and a response over the body
//! cap is a permanent error (not retried forever by the polling daemon).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use aaf_engine::telegram::client::{HttpTelegramApi, TelegramApi};
use aaf_engine::telegram::is_transient_poll_error;

const TOKEN: &str = "112233445:AAF_LocalMockToken_99887766";

/// Reads one HTTP request (headers + Content-Length body) and returns its body.
fn read_request_body(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&buf).into_owned();
        if let Some(idx) = text.find("\r\n\r\n") {
            let len = text[..idx]
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            if buf.len() >= idx + 4 + len {
                return text[idx + 4..idx + 4 + len].to_string();
            }
        }
    }
    String::new()
}

fn serve_once(body: Vec<u8>) -> (String, thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let req = read_request_body(&mut stream);
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&body);
        let _ = stream.flush();
        req
    });
    (format!("http://{addr}"), handle)
}

fn api(base: &str) -> HttpTelegramApi {
    HttpTelegramApi::with_timeouts(base, TOKEN, Duration::from_secs(2), Duration::from_secs(10))
        .unwrap()
}

#[test]
fn get_updates_payload_bounds_the_batch_size() {
    let (base, server) = serve_once(br#"{"ok":true,"result":[]}"#.to_vec());
    assert!(api(&base).get_updates(Some(3), 0).unwrap().is_empty());
    let payload: serde_json::Value = serde_json::from_str(&server.join().unwrap()).unwrap();
    let limit = payload["limit"].as_u64();
    assert!(
        limit.is_some_and(|l| (1..=20).contains(&l)),
        "getUpdates must send a small `limit`: {payload}"
    );
}

#[test]
fn oversized_get_updates_body_is_not_retried_as_transient() {
    let mut body = br#"{"ok":true,"result":[],"pad":""#.to_vec();
    body.resize(body.len() + 2 * 1024 * 1024 + 16, b'x');
    body.extend_from_slice(br#""}"#);
    let (base, server) = serve_once(body);
    let err = api(&base).get_updates(None, 0).unwrap_err();
    let _ = server.join();
    assert!(
        !is_transient_poll_error(&err),
        "a body over the response cap repeats forever if retried: {err:?}"
    );
}
