//! Zero-dependency HTTP/1.1 + SSE server backed by `std::net::TcpListener`.
//!
//! Enforces bounded concurrency (`MAX_CONCURRENT_CONNS`), bounded header/body
//! sizes, socket timeouts, strict security headers, Host/Origin CSRF &
//! DNS-rebinding defenses, and clean non-blocking shutdown.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::{
    HTML_CSP, RequestSecurityContext, SecurityRejection, UiBackend, UiConfig, UiError, parse_query,
    percent_decode, validate_ui_config, verify_request_security, wrap_ok, write_backend_result,
    write_http_response, write_json_error, write_json_response, write_sse_event,
};
use crate::executors::reaper::validate_run_id;
use crate::git::worktree::validate_task_id;

pub const MAX_CONCURRENT_CONNS: usize = 32;
pub const MAX_HEADER_BYTES: usize = 16 * 1024;
pub const MAX_BODY_BYTES: usize = 64 * 1024;

const IO_TIMEOUT: Duration = Duration::from_secs(10);
const SSE_PUSH_INTERVAL: Duration = Duration::from_millis(1500);
const SSE_POLL_SLICE: Duration = Duration::from_millis(100);
const DEFAULT_STREAM_LINES: usize = 120;
const MAX_STREAM_LINES: usize = 500;
const INDEX_HTML: &str = include_str!("index.html");

/// Running Web UI server handle supporting deterministic shutdown and join.
pub struct UiServerHandle {
    addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
}

impl UiServerHandle {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Signals the accept loop and all active SSE streams to stop cleanly.
    pub fn shutdown(&self) {
        if !self.shutdown.swap(true, Ordering::SeqCst) {
            let wake = wake_addr(self.addr);
            let _ = TcpStream::connect_timeout(&wake, Duration::from_millis(200));
        }
    }

    /// Shuts down the listener and waits for the accept thread to exit.
    pub fn join(mut self) {
        self.shutdown();
        if let Some(handle) = self.accept_thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for UiServerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Validates `cfg`, binds the TCP listener, and starts serving requests.
pub fn start_server(cfg: UiConfig, backend: Arc<dyn UiBackend>) -> Result<UiServerHandle, UiError> {
    validate_ui_config(&cfg)?;
    let listener = TcpListener::bind((cfg.host.as_str(), cfg.port))?;
    let addr = listener.local_addr()?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let active = Arc::new(AtomicUsize::new(0));
    let cfg = Arc::new(cfg);

    let accept_shutdown = Arc::clone(&shutdown);
    let accept_thread = thread::Builder::new()
        .name("aaf-web-ui-accept".to_string())
        .spawn(move || accept_loop(listener, addr.port(), cfg, backend, accept_shutdown, active))?;

    Ok(UiServerHandle {
        addr,
        shutdown,
        accept_thread: Some(accept_thread),
    })
}

fn wake_addr(bound: SocketAddr) -> SocketAddr {
    match bound.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), bound.port())
        }
        IpAddr::V6(ip) if ip.is_unspecified() => {
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), bound.port())
        }
        _ => bound,
    }
}

struct ConnGuard(Arc<AtomicUsize>);
impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn try_reserve_conn(active: &Arc<AtomicUsize>) -> Option<ConnGuard> {
    let mut cur = active.load(Ordering::Acquire);
    while cur < MAX_CONCURRENT_CONNS {
        match active.compare_exchange_weak(cur, cur + 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Some(ConnGuard(Arc::clone(active))),
            Err(next) => cur = next,
        }
    }
    None
}

fn accept_loop(
    listener: TcpListener,
    bound_port: u16,
    cfg: Arc<UiConfig>,
    backend: Arc<dyn UiBackend>,
    shutdown: Arc<AtomicBool>,
    active: Arc<AtomicUsize>,
) {
    loop {
        let Ok((mut stream, _)) = listener.accept() else {
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
            thread::sleep(Duration::from_millis(20));
            continue;
        };
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let Some(guard) = try_reserve_conn(&active) else {
            let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
            let _ = write_json_error(&mut stream, 503, "server connection limit reached");
            continue;
        };
        let (cfg_c, backend_c, shutdown_c) = (
            Arc::clone(&cfg),
            Arc::clone(&backend),
            Arc::clone(&shutdown),
        );
        let _ = thread::Builder::new()
            .name("aaf-web-ui-conn".to_string())
            .spawn(move || {
                let _guard = guard;
                let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
                let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
                match read_http_request(&mut stream) {
                    Ok(req) => {
                        dispatch_request(
                            &mut stream,
                            bound_port,
                            &cfg_c,
                            backend_c.as_ref(),
                            &shutdown_c,
                            &req,
                        );
                    }
                    Err(ParseError::Closed) => {}
                    Err(ParseError::Status(code, msg)) => {
                        let _ = write_json_error(&mut stream, code, &msg);
                    }
                }
            });
    }
}

struct HttpRequest {
    method: String,
    raw_path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

enum ParseError {
    Status(u16, String),
    Closed,
}

fn parse_err(code: u16, msg: impl Into<String>) -> ParseError {
    ParseError::Status(code, msg.into())
}

fn read_chunk(
    stream: &mut TcpStream,
    buf: &mut [u8],
    empty_is_closed: bool,
) -> Result<usize, ParseError> {
    match stream.read(buf) {
        Ok(0) if empty_is_closed => Err(ParseError::Closed),
        Ok(0) => Err(parse_err(400, "truncated HTTP request")),
        Ok(n) => Ok(n),
        Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
            Err(parse_err(408, "request timed out"))
        }
        Err(_) => Err(ParseError::Closed),
    }
}

fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest, ParseError> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if buf.len() >= MAX_HEADER_BYTES {
            return Err(parse_err(413, "headers exceed 16 KiB limit"));
        }
        let max_read = (MAX_HEADER_BYTES - buf.len()).min(chunk.len());
        let n = read_chunk(stream, &mut chunk[..max_read], buf.is_empty())?;
        buf.extend_from_slice(&chunk[..n]);
    };

    let text = std::str::from_utf8(&buf[..header_end])
        .map_err(|_| parse_err(400, "non-UTF-8 HTTP headers"))?;
    let mut lines = text.split("\r\n");
    let req_line = lines
        .next()
        .ok_or_else(|| parse_err(400, "missing request line"))?;
    let mut parts = req_line.split_whitespace();
    let (Some(method), Some(target), Some(ver), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(parse_err(400, "malformed HTTP request line"));
    };
    if !ver.starts_with("HTTP/1.") {
        return Err(parse_err(400, "unsupported HTTP version"));
    }

    let mut headers = HashMap::new();
    for line in lines.filter(|l| !l.is_empty()) {
        let (k, v) = line
            .split_once(':')
            .ok_or_else(|| parse_err(400, "malformed header"))?;
        let key = k.trim().to_ascii_lowercase();
        if matches!(
            key.as_str(),
            "host" | "origin" | "authorization" | "content-length"
        ) && headers.contains_key(&key)
        {
            return Err(parse_err(400, format!("duplicate header: {key}")));
        }
        headers.insert(key, v.trim().to_string());
    }
    if headers
        .get("transfer-encoding")
        .is_some_and(|v| !v.is_empty())
    {
        return Err(parse_err(400, "chunked transfer-encoding unsupported"));
    }

    let content_len = match headers.get("content-length") {
        Some(raw) => raw
            .parse::<usize>()
            .map_err(|_| parse_err(400, "invalid Content-Length"))?,
        None => 0,
    };
    if content_len > MAX_BODY_BYTES {
        return Err(parse_err(413, "body exceeds 64 KiB limit"));
    }

    let mut body = Vec::with_capacity(content_len);
    let body_start = header_end + 4;
    if body_start < buf.len() {
        let take = (buf.len() - body_start).min(content_len);
        body.extend_from_slice(&buf[body_start..body_start + take]);
    }
    while body.len() < content_len {
        let need = (content_len - body.len()).min(chunk.len());
        let n = read_chunk(stream, &mut chunk[..need], false)?;
        body.extend_from_slice(&chunk[..n]);
    }

    let (raw_path, raw_query) = target.split_once('?').unwrap_or((target, ""));
    Ok(HttpRequest {
        method: method.to_string(),
        raw_path: raw_path.to_string(),
        query: parse_query(raw_query),
        headers,
        body,
    })
}

enum Route {
    Index,
    Health,
    Meta,
    Status,
    Board,
    TaskDetail(String),
    Handover(String),
    ApprovePreview(String),
    StreamTail(String),
    Events,
    ApproveCommit,
    Action,
    MethodNotAllowed,
    BadRequest(&'static str),
    NotFound,
}

fn match_route(method: &str, path: &str) -> Route {
    let exact = |want: &str, r: Route| {
        if method == want {
            r
        } else {
            Route::MethodNotAllowed
        }
    };
    match path {
        "/" => return exact("GET", Route::Index),
        "/api/health" => return exact("GET", Route::Health),
        "/api/meta" => return exact("GET", Route::Meta),
        "/api/status" => return exact("GET", Route::Status),
        "/api/board" => return exact("GET", Route::Board),
        "/api/events" => return exact("GET", Route::Events),
        "/api/approve" => return exact("POST", Route::ApproveCommit),
        "/api/action" => return exact("POST", Route::Action),
        _ => {}
    }
    type RouteCtor = fn(String) -> Route;
    let prefixes: [(&str, RouteCtor); 4] = [
        ("/api/task/", Route::TaskDetail),
        ("/api/handover/", Route::Handover),
        ("/api/approve/preview/", Route::ApprovePreview),
        ("/api/streams/", Route::StreamTail),
    ];
    for (prefix, build) in prefixes {
        if let Some(raw_id) = path.strip_prefix(prefix)
            && !raw_id.is_empty()
            && !raw_id.contains('/')
        {
            if method != "GET" {
                return Route::MethodNotAllowed;
            }
            return match percent_decode(raw_id) {
                Some(id) if !id.is_empty() => build(id),
                _ => Route::BadRequest("invalid URL percent encoding"),
            };
        }
    }
    Route::NotFound
}

fn dispatch_request(
    stream: &mut TcpStream,
    bound_port: u16,
    cfg: &UiConfig,
    backend: &dyn UiBackend,
    shutdown: &AtomicBool,
    req: &HttpRequest,
) {
    let route = match_route(&req.method, &req.raw_path);
    let sec_ctx = RequestSecurityContext {
        method: &req.method,
        path: &req.raw_path,
        host_header: req.headers.get("host").map(String::as_str),
        origin_header: req.headers.get("origin").map(String::as_str),
        auth_header: req.headers.get("authorization").map(String::as_str),
        query_token: req.query.get("token").map(String::as_str),
        require_auth: !matches!(route, Route::Index | Route::Health),
        allow_query_token: matches!(route, Route::Events),
        is_mutating: matches!(route, Route::ApproveCommit | Route::Action),
    };
    if let Err(rej) = verify_request_security(cfg, bound_port, &sec_ctx) {
        let (code, msg) = match rej {
            SecurityRejection::Unauthorized(m) => (401, m),
            SecurityRejection::Forbidden(m) => (403, m),
        };
        let _ = write_json_error(stream, code, &msg);
        return;
    }

    match route {
        Route::Index => {
            let csp = format!("Content-Security-Policy: {HTML_CSP}\r\n");
            let _ = write_http_response(
                stream,
                200,
                "text/html; charset=utf-8",
                &csp,
                INDEX_HTML.as_bytes(),
            );
        }
        Route::Health => {
            let _ = write_json_response(stream, 200, &json!({ "ok": true }));
        }
        Route::Meta => write_backend_result(stream, backend.meta_json()),
        Route::Status => write_backend_result(stream, backend.status_json()),
        Route::Board => {
            let st = req
                .query
                .get("state")
                .map(String::as_str)
                .filter(|s| !s.is_empty());
            write_backend_result(stream, backend.board_json(st));
        }
        Route::TaskDetail(id) => with_task_id(stream, &id, |id| backend.task_detail_json(id)),
        Route::Handover(id) => with_task_id(stream, &id, |id| backend.handover_json(id)),
        Route::ApprovePreview(id) => {
            with_task_id(stream, &id, |id| backend.approve_preview_json(id))
        }
        Route::StreamTail(run_id) => match validate_run_id(&run_id) {
            Ok(()) => {
                let max = req
                    .query
                    .get("max_lines")
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(DEFAULT_STREAM_LINES)
                    .clamp(1, MAX_STREAM_LINES);
                let res = backend
                    .stream_tail_lines(&run_id, max)
                    .map(|lines| json!({ "ok": true, "run_id": run_id, "lines": lines }));
                write_backend_result(stream, res);
            }
            Err(e) => {
                let _ = write_json_error(stream, 400, &format!("invalid run id: {e}"));
            }
        },
        Route::Events => serve_sse(stream, backend, shutdown, req.query.get("run_id")),
        Route::ApproveCommit => handle_approve_post(stream, backend, &req.body),
        Route::Action => handle_action_post(stream, backend, &req.body),
        Route::BadRequest(msg) => {
            let _ = write_json_error(stream, 400, msg);
        }
        Route::MethodNotAllowed => {
            let _ = write_json_error(stream, 405, "method not allowed");
        }
        Route::NotFound => {
            let _ = write_json_error(stream, 404, "not found");
        }
    }
}

fn with_task_id(stream: &mut TcpStream, id: &str, f: impl FnOnce(&str) -> Result<Value, String>) {
    match validate_task_id(id) {
        Ok(()) => write_backend_result(stream, f(id)),
        Err(e) => {
            let _ = write_json_error(stream, 400, &format!("invalid task id: {e}"));
        }
    }
}

fn parse_json_body<T: DeserializeOwned>(stream: &mut TcpStream, body: &[u8]) -> Option<T> {
    match serde_json::from_slice(body) {
        Ok(v) => Some(v),
        Err(e) => {
            let _ = write_json_error(stream, 400, &format!("invalid JSON body: {e}"));
            None
        }
    }
}

#[derive(Deserialize)]
struct ApproveBody {
    task_id: String,
    lock_fp: String,
    #[serde(default)]
    reviewer: String,
}

fn handle_approve_post(stream: &mut TcpStream, backend: &dyn UiBackend, body: &[u8]) {
    let Some(p) = parse_json_body::<ApproveBody>(stream, body) else {
        return;
    };
    if let Err(e) = validate_task_id(&p.task_id) {
        let _ = write_json_error(stream, 400, &format!("invalid task id: {e}"));
        return;
    }
    let lock_fp = p.lock_fp.trim();
    if lock_fp.is_empty() {
        let _ = write_json_error(stream, 400, "lock_fp is required");
        return;
    }
    let reviewer = match p.reviewer.trim() {
        "" => "web-ui",
        other => other,
    };
    write_backend_result(
        stream,
        backend.approve_commit(&p.task_id, lock_fp, reviewer),
    );
}

#[derive(Deserialize)]
struct ActionBody {
    action: String,
    #[serde(default)]
    task_id: String,
    #[serde(default)]
    note: String,
}

fn handle_action_post(stream: &mut TcpStream, backend: &dyn UiBackend, body: &[u8]) {
    let Some(p) = parse_json_body::<ActionBody>(stream, body) else {
        return;
    };
    let action = p.action.trim().to_ascii_lowercase();
    if !matches!(action.as_str(), "unblock" | "rework" | "steer" | "unfreeze") {
        let _ = write_json_error(stream, 400, "unsupported action");
        return;
    }
    if action != "unfreeze"
        && let Err(e) = validate_task_id(&p.task_id)
    {
        let _ = write_json_error(stream, 400, &format!("invalid task id: {e}"));
        return;
    }
    write_backend_result(
        stream,
        backend.mutate_task(&action, p.task_id.trim(), p.note.trim()),
    );
}

fn serve_sse(
    stream: &mut TcpStream,
    backend: &dyn UiBackend,
    shutdown: &AtomicBool,
    run_id_param: Option<&String>,
) {
    let run_id = run_id_param
        .map(String::as_str)
        .filter(|id| !id.is_empty() && validate_run_id(id).is_ok());
    let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\n\
        Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\n\
        Referrer-Policy: no-referrer\r\nX-Accel-Buffering: no\r\nConnection: keep-alive\r\n\r\n";
    if stream.write_all(headers.as_bytes()).is_err() || stream.flush().is_err() {
        return;
    }

    let mut last_push = Instant::now()
        .checked_sub(SSE_PUSH_INTERVAL)
        .unwrap_or_else(Instant::now);
    while !shutdown.load(Ordering::SeqCst) {
        if last_push.elapsed() >= SSE_PUSH_INTERVAL {
            if let Ok(status) = backend.status_json()
                && write_sse_event(stream, "status", &wrap_ok(status)).is_err()
            {
                break;
            }
            if let Some(rid) = run_id
                && let Ok(lines) = backend.stream_tail_lines(rid, DEFAULT_STREAM_LINES)
                && write_sse_event(stream, "stream", &json!({ "run_id": rid, "lines": lines }))
                    .is_err()
            {
                break;
            }
            if stream.write_all(b": keepalive\n\n").is_err() || stream.flush().is_err() {
                break;
            }
            last_push = Instant::now();
        }
        thread::sleep(SSE_POLL_SLICE);
    }
}
