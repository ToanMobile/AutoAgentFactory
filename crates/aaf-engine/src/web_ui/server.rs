//! Zero-dependency HTTP/1.1 + SSE server backed by `std::net::TcpListener`.
//!
//! Enforces bounded concurrency (`MAX_CONCURRENT_CONNS`), bounded header/body
//! sizes, socket timeouts, strict security headers, Host/Origin CSRF &
//! DNS-rebinding defenses, and clean non-blocking shutdown.

use std::collections::HashMap;
use std::io::{ErrorKind, Read};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::routes::{Route, action_needs_task_id, match_route, normalize_action};
use super::sse::{serve_sse, try_reserve_limit};
use super::{
    APPROVE_DISABLED_MSG, HTML_CSP, RequestSecurityContext, SecurityRejection, UiBackend, UiConfig,
    UiError, backend_error_status, parse_query, validate_ui_config, verify_request_security,
    with_meta_policy, write_backend_result, write_http_response, write_json_error,
    write_json_response,
};
use crate::executors::reaper::validate_run_id;
use crate::git::worktree::validate_task_id;

pub const MAX_CONCURRENT_CONNS: usize = 32;
pub const MAX_HEADER_BYTES: usize = 16 * 1024;
pub const MAX_BODY_BYTES: usize = 64 * 1024;

const IO_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_REPORT_HOURS: u64 = 24 * 365;
const DEFAULT_STREAM_LINES: usize = 120;
const MAX_STREAM_LINES: usize = 500;
const INDEX_HTML: &str = include_str!("index.html");
const STUDIO_HTML: &str = include_str!("studio.html");

pub(super) type ShutdownState = Arc<(Mutex<bool>, Condvar)>;

pub(super) fn is_shutdown(state: &ShutdownState) -> bool {
    *state.0.lock().unwrap_or_else(|e| e.into_inner())
}

pub(super) fn wait_for_shutdown(state: &ShutdownState, timeout: Duration) -> bool {
    let guard = state.0.lock().unwrap_or_else(|e| e.into_inner());
    let (guard, _) = state
        .1
        .wait_timeout_while(guard, timeout, |stopped| !*stopped)
        .unwrap_or_else(|e| e.into_inner());
    *guard
}

/// Running Web UI server handle supporting deterministic shutdown and join.
pub struct UiServerHandle {
    addr: SocketAddr,
    shutdown: ShutdownState,
    accept_thread: Option<JoinHandle<()>>,
}

impl UiServerHandle {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Signals the accept loop and all active SSE streams to stop cleanly.
    pub fn shutdown(&self) {
        let changed = {
            let mut flag = self.shutdown.0.lock().unwrap_or_else(|e| e.into_inner());
            let prev = *flag;
            *flag = true;
            !prev
        };
        if changed {
            self.shutdown.1.notify_all();
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
    let shutdown: ShutdownState = Arc::new((Mutex::new(false), Condvar::new()));
    let active = Arc::new(AtomicUsize::new(0));
    let sse_active = Arc::new(AtomicUsize::new(0));
    let cfg = Arc::new(cfg);

    let accept_shutdown = Arc::clone(&shutdown);
    let accept_thread = thread::Builder::new()
        .name("aaf-web-ui-accept".to_string())
        .spawn(move || {
            accept_loop(
                listener,
                addr.port(),
                cfg,
                backend,
                accept_shutdown,
                active,
                sse_active,
            )
        })?;

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

fn accept_loop(
    listener: TcpListener,
    bound_port: u16,
    cfg: Arc<UiConfig>,
    backend: Arc<dyn UiBackend>,
    shutdown: ShutdownState,
    active: Arc<AtomicUsize>,
    sse_active: Arc<AtomicUsize>,
) {
    loop {
        let Ok((mut stream, _)) = listener.accept() else {
            if is_shutdown(&shutdown) {
                break;
            }
            thread::sleep(Duration::from_millis(20));
            continue;
        };
        if is_shutdown(&shutdown) {
            break;
        }
        let Some(guard) = try_reserve_limit(&active, MAX_CONCURRENT_CONNS) else {
            let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
            let _ = write_json_error(&mut stream, 503, "server connection limit reached");
            continue;
        };
        let (cfg_c, backend_c, shutdown_c, sse_c) = (
            Arc::clone(&cfg),
            Arc::clone(&backend),
            Arc::clone(&shutdown),
            Arc::clone(&sse_active),
        );
        let _ = thread::Builder::new()
            .name("aaf-web-ui-conn".to_string())
            .spawn(move || {
                let _guard = guard;
                let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
                let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
                match read_http_request(&mut stream) {
                    Ok(req) => dispatch_request(
                        &mut stream,
                        bound_port,
                        &cfg_c,
                        backend_c.as_ref(),
                        &shutdown_c,
                        &sse_c,
                        &req,
                    ),
                    Err(ParseError::Closed) => {}
                    Err(ParseError::Status(code, msg)) => {
                        let _ = write_json_error(&mut stream, code, &msg);
                    }
                }
            });
    }
}

pub(super) struct HttpRequest {
    method: String,
    raw_path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

pub(super) enum ParseError {
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
    deadline: Instant,
) -> Result<usize, ParseError> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| parse_err(408, "request timed out"))?;
    let _ = stream.set_read_timeout(Some(remaining));
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
    read_http_request_with_timeout(stream, IO_TIMEOUT)
}

pub(super) fn read_http_request_with_timeout(
    stream: &mut TcpStream,
    timeout: Duration,
) -> Result<HttpRequest, ParseError> {
    let deadline = Instant::now() + timeout;
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
        let n = read_chunk(stream, &mut chunk[..max_read], buf.is_empty(), deadline)?;
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
        let n = read_chunk(stream, &mut chunk[..need], false, deadline)?;
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

fn send_err(stream: &mut TcpStream, code: u16, msg: &str) {
    let _ = write_json_error(stream, code, msg);
}

fn dispatch_request(
    stream: &mut TcpStream,
    bound_port: u16,
    cfg: &UiConfig,
    backend: &dyn UiBackend,
    shutdown: &ShutdownState,
    sse_active: &Arc<AtomicUsize>,
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
        require_auth: !matches!(
            route,
            Route::Index | Route::Studio | Route::Static(_) | Route::Health
        ),
        allow_query_token: matches!(route, Route::Events),
        is_mutating: matches!(
            route,
            Route::ApproveCommit | Route::Action | Route::CreateTask
        ),
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
        Route::Index => write_html(stream, INDEX_HTML),
        Route::Studio => write_html(stream, STUDIO_HTML),
        Route::Static(asset) => super::pwa::serve_static(stream, asset),
        Route::Report => {
            let since = req
                .query
                .get("since")
                .or_else(|| req.query.get("hours"))
                .and_then(|s| s.parse::<u64>().ok())
                .filter(|&h| h > 0)
                .map(|h| h.min(MAX_REPORT_HOURS));
            let repo = req
                .query
                .get("repo")
                .map(String::as_str)
                .filter(|s| !s.is_empty());
            write_backend_result(stream, backend.report_json_scoped(since, repo));
        }
        Route::Doctor => {
            let repo = req
                .query
                .get("repo")
                .map(String::as_str)
                .filter(|s| !s.is_empty());
            write_backend_result(stream, backend.doctor_json_scoped(repo));
        }
        Route::Health => {
            let _ = write_json_response(stream, 200, &json!({ "ok": true }));
        }
        Route::Meta => write_backend_result(stream, with_meta_policy(backend.meta_json(), cfg)),
        Route::Status => write_backend_result(stream, backend.status_json()),
        Route::Accounts => {
            let refresh = req
                .query
                .get("refresh")
                .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes"));
            write_backend_result(stream, backend.accounts_json(refresh));
        }
        Route::Board => {
            let st = req
                .query
                .get("state")
                .map(String::as_str)
                .filter(|s| !s.is_empty());
            write_backend_result(stream, backend.board_json(st));
        }
        Route::SopRuns => write_backend_result(stream, backend.sop_runs_json()),
        Route::TaskDetail(id) => with_task_id(stream, &id, |id| backend.task_detail_json(id)),
        Route::TaskDiff(id) => with_task_id(stream, &id, |id| backend.task_diff_json(id)),
        Route::TaskPipeline(id) => with_task_id(stream, &id, |id| backend.task_pipeline_json(id)),
        Route::TaskProof(id) => with_task_id(stream, &id, |id| backend.task_proof_json(id)),
        Route::ProofImage(id, idx) => match validate_task_id(&id) {
            Ok(()) => match backend.task_proof_image(&id, idx) {
                Ok(png) => {
                    let _ = write_http_response(stream, 200, "image/png", "", &png);
                }
                Err(msg) => send_err(stream, backend_error_status(&msg), &msg),
            },
            Err(e) => send_err(stream, 400, &format!("invalid task id: {e}")),
        },
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
            Err(e) => send_err(stream, 400, &format!("invalid run id: {e}")),
        },
        Route::Events => serve_sse(
            stream,
            backend,
            shutdown,
            sse_active,
            req.query.get("run_id"),
        ),
        Route::CreateTask => {
            if let Some(val) = parse_json_body::<Value>(stream, &req.body) {
                write_backend_result(stream, backend.create_task_json(&val));
            }
        }
        Route::ApproveCommit if !cfg.effective_allow_approve() => {
            send_err(stream, 403, APPROVE_DISABLED_MSG);
        }
        Route::ApproveCommit => handle_approve_post(stream, backend, &req.body),
        Route::Action => handle_action_post(stream, backend, &req.body),
        Route::BadRequest(msg) => send_err(stream, 400, msg),
        Route::MethodNotAllowed => send_err(stream, 405, "method not allowed"),
        Route::NotFound => send_err(stream, 404, "not found"),
    }
}

fn write_html(stream: &mut TcpStream, html: &str) {
    let csp = format!("Content-Security-Policy: {HTML_CSP}\r\n");
    let _ = write_http_response(
        stream,
        200,
        "text/html; charset=utf-8",
        &csp,
        html.as_bytes(),
    );
}

fn with_task_id(stream: &mut TcpStream, id: &str, f: impl FnOnce(&str) -> Result<Value, String>) {
    match validate_task_id(id) {
        Ok(()) => write_backend_result(stream, f(id)),
        Err(e) => send_err(stream, 400, &format!("invalid task id: {e}")),
    }
}

fn parse_json_body<T: DeserializeOwned>(stream: &mut TcpStream, body: &[u8]) -> Option<T> {
    match serde_json::from_slice(body) {
        Ok(v) => Some(v),
        Err(e) => {
            send_err(stream, 400, &format!("invalid JSON body: {e}"));
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
        return send_err(stream, 400, &format!("invalid task id: {e}"));
    }
    let lock_fp = p.lock_fp.trim();
    if lock_fp.is_empty() {
        return send_err(stream, 400, "lock_fp is required");
    }
    let rev = match p.reviewer.trim() {
        "" => "web-ui",
        other => other,
    };
    write_backend_result(stream, backend.approve_commit(&p.task_id, lock_fp, rev));
}

#[derive(Deserialize)]
struct ActionBody {
    action: String,
    #[serde(default)]
    task_id: String,
    #[serde(default)]
    note: String,
    #[serde(default)]
    comments: Vec<Value>,
}

fn handle_action_post(stream: &mut TcpStream, backend: &dyn UiBackend, body: &[u8]) {
    let Some(p) = parse_json_body::<ActionBody>(stream, body) else {
        return;
    };
    let Some(normalized) = normalize_action(&p.action) else {
        return send_err(stream, 400, "unsupported action");
    };
    if normalized == "stop_run" {
        if let Err(e) = validate_run_id(p.task_id.trim()) {
            return send_err(stream, 400, &format!("invalid run id: {e}"));
        }
    } else if action_needs_task_id(normalized)
        && let Err(e) = validate_task_id(&p.task_id)
    {
        return send_err(stream, 400, &format!("invalid task id: {e}"));
    }
    let res = if p.comments.is_empty() {
        backend.mutate_task(normalized, p.task_id.trim(), p.note.trim())
    } else {
        backend.mutate_task_with_comments(normalized, p.task_id.trim(), p.note.trim(), &p.comments)
    };
    write_backend_result(stream, res);
}
