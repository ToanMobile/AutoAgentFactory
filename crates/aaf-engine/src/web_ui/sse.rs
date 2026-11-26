//! Bounded SSE (`/api/events`) handler and connection-limit guard for `web_ui`.

use std::io::Write;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::json;

use super::server::{ShutdownState, is_shutdown, wait_for_shutdown};
use super::{UiBackend, wrap_ok, write_json_error, write_sse_event};
use crate::executors::reaper::validate_run_id;

pub const MAX_SSE_CONNS: usize = 16;
const SSE_IO_TIMEOUT: Duration = Duration::from_secs(3);
const SSE_PUSH_INTERVAL: Duration = Duration::from_millis(1500);
const DEFAULT_STREAM_LINES: usize = 120;

pub(super) struct ConnGuard(Arc<AtomicUsize>);

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(super) fn try_reserve_limit(active: &Arc<AtomicUsize>, limit: usize) -> Option<ConnGuard> {
    let mut cur = active.load(Ordering::Acquire);
    while cur < limit {
        match active.compare_exchange_weak(cur, cur + 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Some(ConnGuard(Arc::clone(active))),
            Err(next) => cur = next,
        }
    }
    None
}

pub(super) fn serve_sse(
    stream: &mut TcpStream,
    backend: &dyn UiBackend,
    shutdown: &ShutdownState,
    sse_active: &Arc<AtomicUsize>,
    run_id_param: Option<&String>,
) {
    let Some(_sse_guard) = try_reserve_limit(sse_active, MAX_SSE_CONNS) else {
        let _ = write_json_error(stream, 503, "SSE stream limit reached");
        return;
    };
    let _ = stream.set_write_timeout(Some(SSE_IO_TIMEOUT));
    let run_id = run_id_param
        .map(String::as_str)
        .filter(|id| !id.is_empty() && validate_run_id(id).is_ok());
    let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\n\
        Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\n\
        Referrer-Policy: no-referrer\r\nX-Accel-Buffering: no\r\nConnection: keep-alive\r\n\r\n";
    if stream.write_all(headers.as_bytes()).is_err() || stream.flush().is_err() {
        return;
    }

    let mut last_stream: Option<Vec<String>> = None;
    while !is_shutdown(shutdown) {
        if let Ok(status) = backend.status_json()
            && write_sse_event(stream, "status", &wrap_ok(status)).is_err()
        {
            break;
        }
        if let Some(rid) = run_id
            && let Ok(lines) = backend.stream_tail_lines(rid, DEFAULT_STREAM_LINES)
            && last_stream.as_ref() != Some(&lines)
        {
            let ev = json!({ "run_id": rid, "lines": &lines });
            if write_sse_event(stream, "stream", &ev).is_err() {
                break;
            }
            last_stream = Some(lines);
        }
        if stream.write_all(b": keepalive\n\n").is_err() || stream.flush().is_err() {
            break;
        }
        if wait_for_shutdown(shutdown, SSE_PUSH_INTERVAL) {
            break;
        }
    }
}
