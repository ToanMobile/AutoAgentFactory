//! Zero-dependency authenticated local/LAN Web & Mobile Cockpit (HTTP + SSE + QR).
//!
//! Security invariants:
//! - Non-loopback binds without an explicit bearer token (>= 16 chars) are
//!   refused (`UiError::InsecureLanBind`).
//! - Bearer tokens are compared in constant time (`constant_time_eq`).
//! - Query-string tokens (`?token=`) are accepted ONLY on `GET /api/events`
//!   (where browser `EventSource` cannot send custom headers) and NEVER on
//!   mutating `POST` endpoints or JSON API routes.
//! - Foreign `Host` headers on loopback binds (DNS rebinding) and cross-origin
//!   `Origin` headers (CSRF) are rejected with HTTP 403.
//! - Strict security response headers (`X-Content-Type-Options`,
//!   `X-Frame-Options`, `Referrer-Policy`, `Cache-Control`, and HTML CSP) are
//!   attached to every response.

pub mod qr;
pub mod server;
#[cfg(test)]
mod tests;

use std::collections::hash_map::RandomState;
use std::fmt::Write as _;
use std::fs::File;
use std::hash::{BuildHasher, Hasher};
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use sha2::{Digest, Sha256};

pub use qr::{pairing_url, render_pairing_card, render_qr_ascii};
pub use server::{UiServerHandle, start_server};

pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 4319;
pub const MIN_LAN_TOKEN_LEN: usize = 16;
pub const APPROVE_DISABLED_MSG: &str = "POST /api/approve is disabled by default; start `aaf ui --allow-approve` or use interactive `aaf approve` on a TTY";

pub const HTML_CSP: &str = "default-src 'self'; script-src 'self' 'unsafe-inline'; \
    style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; \
    frame-ancestors 'none'";

static TOKEN_SEQ: AtomicU64 = AtomicU64::new(1);

/// Runtime configuration for the local/LAN Web & Mobile Cockpit server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiConfig {
    pub host: String,
    pub port: u16,
    pub token: String,
    pub allow_mutating: bool,
    pub allow_approve: bool,
    pub lan_mode: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            host: DEFAULT_HOST.to_string(),
            port: DEFAULT_PORT,
            token: generate_token(),
            allow_mutating: true,
            allow_approve: false,
            lan_mode: false,
        }
    }
}

impl UiConfig {
    /// Builds and validates a [`UiConfig`], auto-generating a 32-hex bearer
    /// token only when binding to a loopback host without an explicit key.
    /// `allow_approve` defaults to `false`; call [`Self::with_allow_approve`] to opt in.
    pub fn resolve(
        host: impl Into<String>,
        port: u16,
        explicit_token: Option<String>,
        allow_mutating: bool,
    ) -> Result<Self, UiError> {
        let host = host.into().trim().to_string();
        let loopback = is_loopback_host(&host);
        let token = match explicit_token
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
        {
            Some(t) => t,
            None if loopback => generate_token(),
            None => String::new(),
        };
        let cfg = Self {
            host,
            port,
            token,
            allow_mutating,
            allow_approve: false,
            lan_mode: !loopback,
        };
        validate_ui_config(&cfg)?;
        Ok(cfg)
    }

    /// Opts in to `POST /api/approve`; fail-closed (`false`) when `allow_mutating` is `false`.
    pub fn with_allow_approve(mut self, allow: bool) -> Self {
        self.allow_approve = allow && self.allow_mutating;
        self
    }

    /// Effective `POST /api/approve` policy (always `false` in read-only mode).
    pub fn effective_allow_approve(&self) -> bool {
        self.allow_mutating && self.allow_approve
    }
}

/// Errors produced when validating configuration or starting the Web UI server.
#[derive(Debug, thiserror::Error)]
pub enum UiError {
    #[error(
        "refusing non-loopback bind on '{host}' without an explicit bearer token (>= {min_len} chars); set AAF_UI_TOKEN (or pass --key)"
    )]
    InsecureLanBind { host: String, min_len: usize },
    #[error("invalid bind host '{host}'")]
    InvalidHost { host: String },
    #[error("bearer token is empty or contains invalid characters")]
    InvalidToken,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Reason a request was rejected by [`verify_request_security`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecurityRejection {
    Unauthorized(String),
    Forbidden(String),
}

/// Request metadata inspected by [`verify_request_security`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestSecurityContext<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub host_header: Option<&'a str>,
    pub origin_header: Option<&'a str>,
    pub auth_header: Option<&'a str>,
    pub query_token: Option<&'a str>,
    pub require_auth: bool,
    pub allow_query_token: bool,
    pub is_mutating: bool,
}

/// Backend contract implemented by the CLI layer (or test fakes) for the Web UI.
pub trait UiBackend: Send + Sync {
    fn status_json(&self) -> Result<Value, String>;
    fn board_json(&self, state: Option<&str>) -> Result<Value, String>;
    fn task_detail_json(&self, task_id: &str) -> Result<Value, String>;
    fn handover_json(&self, task_id: &str) -> Result<Value, String>;
    fn approve_preview_json(&self, task_id: &str) -> Result<Value, String>;
    fn approve_commit(
        &self,
        task_id: &str,
        expected_lock_fp: &str,
        reviewer: &str,
    ) -> Result<Value, String>;
    fn mutate_task(&self, action: &str, task_id: &str, note: &str) -> Result<Value, String>;
    fn stream_tail_lines(&self, run_id: &str, max_lines: usize) -> Result<Vec<String>, String>;
    fn meta_json(&self) -> Result<Value, String>;
}

/// Returns `true` strictly for loopback hosts (`127.0.0.1`, `localhost`, `::1`).
pub fn is_loopback_host(host: &str) -> bool {
    let clean = strip_ipv6_brackets(host.trim());
    matches!(
        clean.to_ascii_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "::1"
    )
}

/// Validates bind host, LAN authentication policy, and bearer token hygiene.
pub fn validate_ui_config(cfg: &UiConfig) -> Result<(), UiError> {
    let host = cfg.host.trim();
    if host.is_empty()
        || host
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control() || matches!(b, b'/' | b'@'))
    {
        return Err(UiError::InvalidHost {
            host: cfg.host.clone(),
        });
    }
    let loopback = is_loopback_host(host);
    let token = cfg.token.trim();
    let token_clean = !token.is_empty()
        && token
            .bytes()
            .all(|b| b.is_ascii_graphic() && !b.is_ascii_control());
    if !loopback || cfg.lan_mode {
        if !token_clean || token.len() < MIN_LAN_TOKEN_LEN {
            return Err(UiError::InsecureLanBind {
                host: host.to_string(),
                min_len: MIN_LAN_TOKEN_LEN,
            });
        }
    } else if !token_clean {
        return Err(UiError::InvalidToken);
    }
    Ok(())
}

/// Generates a 32-character lowercase hex bearer token (128 bits of entropy).
pub fn generate_token() -> String {
    let mut entropy = [0u8; 32];
    if let Ok(mut f) = File::open("/dev/urandom") {
        let _ = f.read_exact(&mut entropy);
    }
    let seq = TOKEN_SEQ.fetch_add(1, Ordering::Relaxed);
    let r1 = RandomState::new().build_hasher().finish();
    let r2 = RandomState::new().build_hasher().finish();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0u128, |d| d.as_nanos());

    let mut hasher = Sha256::new();
    hasher.update(entropy);
    hasher.update(r1.to_le_bytes());
    hasher.update(r2.to_le_bytes());
    hasher.update(seq.to_le_bytes());
    hasher.update(nanos.to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    let digest = hasher.finalize();

    let mut hex = String::with_capacity(32);
    for byte in &digest[..16] {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Constant-time byte slice equality whose loop iterations depend only on `b`.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for (idx, &expected) in b.iter().enumerate() {
        let actual = a.get(idx).copied().unwrap_or(0);
        diff |= usize::from(actual ^ expected);
    }
    diff == 0
}

/// Enforces Host (DNS rebinding), Origin (CSRF), Bearer auth, and read-only policy.
pub fn verify_request_security(
    cfg: &UiConfig,
    bound_port: u16,
    req: &RequestSecurityContext<'_>,
) -> Result<(), SecurityRejection> {
    let parsed_host = match req.host_header {
        Some(raw) => Some(verify_host_header(cfg, bound_port, raw)?),
        None => None,
    };
    if let Some(origin) = req.origin_header {
        verify_origin_header(cfg, bound_port, origin, parsed_host.as_deref())?;
    }

    if req.require_auth {
        let header_token = req.auth_header.and_then(extract_bearer_token);
        let sse_query_allowed = req.allow_query_token
            && !req.is_mutating
            && req.method.eq_ignore_ascii_case("GET")
            && req.path == "/api/events";
        let candidate = header_token.or(if sse_query_allowed {
            req.query_token.filter(|t| !t.is_empty())
        } else {
            None
        });
        let Some(token) = candidate else {
            return Err(SecurityRejection::Unauthorized(
                "missing bearer authorization token".to_string(),
            ));
        };
        if !constant_time_eq(token.as_bytes(), cfg.token.as_bytes()) {
            return Err(SecurityRejection::Unauthorized(
                "invalid bearer authorization token".to_string(),
            ));
        }
    }

    if req.is_mutating && !cfg.allow_mutating {
        return Err(SecurityRejection::Forbidden(
            "server is running in read-only mode".to_string(),
        ));
    }
    if req.method.eq_ignore_ascii_case("POST")
        && req.path == "/api/approve"
        && !cfg.effective_allow_approve()
    {
        return Err(SecurityRejection::Forbidden(
            APPROVE_DISABLED_MSG.to_string(),
        ));
    }
    Ok(())
}

fn extract_bearer_token(header: &str) -> Option<&str> {
    let trimmed = header.trim();
    let (scheme, rest) = trimmed.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = rest.trim();
    if token.is_empty() || token.bytes().any(|b| b.is_ascii_whitespace()) {
        return None;
    }
    Some(token)
}

fn verify_host_header(
    cfg: &UiConfig,
    bound_port: u16,
    raw: &str,
) -> Result<String, SecurityRejection> {
    let (host, port) = parse_authority(raw)
        .ok_or_else(|| SecurityRejection::Forbidden("malformed Host header".to_string()))?;
    if let Some(p) = port
        && p != bound_port
    {
        return Err(SecurityRejection::Forbidden(
            "Host header port does not match listener port".to_string(),
        ));
    }
    if !cfg.lan_mode && !is_loopback_host(&host) {
        return Err(SecurityRejection::Forbidden(
            "foreign Host header rejected on loopback bind".to_string(),
        ));
    }
    Ok(host)
}

fn verify_origin_header(
    cfg: &UiConfig,
    bound_port: u16,
    raw: &str,
    request_host: Option<&str>,
) -> Result<(), SecurityRejection> {
    let trimmed = raw.trim();
    let (default_port, authority) = if let Some(rest) = trimmed.strip_prefix("http://") {
        (80u16, rest)
    } else if let Some(rest) = trimmed.strip_prefix("https://") {
        (443u16, rest)
    } else {
        return Err(SecurityRejection::Forbidden(
            "cross-origin request rejected".to_string(),
        ));
    };
    if authority.contains('/') || authority.contains('?') || authority.contains('#') {
        return Err(SecurityRejection::Forbidden(
            "malformed Origin header".to_string(),
        ));
    }
    let (origin_host, origin_port) = parse_authority(authority)
        .ok_or_else(|| SecurityRejection::Forbidden("malformed Origin header".to_string()))?;
    if origin_port.unwrap_or(default_port) != bound_port {
        return Err(SecurityRejection::Forbidden(
            "Origin port does not match server port".to_string(),
        ));
    }
    let allowed = if !cfg.lan_mode {
        is_loopback_host(&origin_host)
    } else {
        let cfg_host = strip_ipv6_brackets(cfg.host.trim());
        let is_wildcard = matches!(cfg_host, "0.0.0.0" | "::");
        if is_wildcard {
            request_host.is_some_and(|h| h.eq_ignore_ascii_case(&origin_host))
        } else {
            origin_host.eq_ignore_ascii_case(cfg_host)
                || request_host.is_some_and(|h| h.eq_ignore_ascii_case(&origin_host))
        }
    };
    if !allowed {
        return Err(SecurityRejection::Forbidden(
            "cross-origin request rejected".to_string(),
        ));
    }
    Ok(())
}

fn parse_authority(raw: &str) -> Option<(String, Option<u16>)> {
    let s = raw.trim();
    if s.is_empty()
        || s.bytes()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control() || matches!(b, b'/' | b'@'))
    {
        return None;
    }
    if let Some(rest) = s.strip_prefix('[') {
        let (addr, after) = rest.split_once(']')?;
        if addr.is_empty() {
            return None;
        }
        let port = if after.is_empty() {
            None
        } else {
            Some(after.strip_prefix(':')?.parse::<u16>().ok()?)
        };
        return Some((addr.to_ascii_lowercase(), port));
    }
    if let Some((h, p)) = s.rsplit_once(':')
        && !h.contains(':')
    {
        if h.is_empty() {
            return None;
        }
        let port = p.parse::<u16>().ok()?;
        return Some((h.to_ascii_lowercase(), Some(port)));
    }
    Some((s.to_ascii_lowercase(), None))
}

fn strip_ipv6_brackets(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host)
}

pub(crate) fn parse_query(raw: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for pair in raw.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if let (Some(dk), Some(dv)) = (percent_decode(k), percent_decode(v)) {
            map.insert(dk, dv);
        }
    }
    map
}

pub(crate) fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                let hi = hex_val(*bytes.get(i + 1)?)?;
                let lo = hex_val(*bytes.get(i + 2)?)?;
                out.push((hi << 4) | lo);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub(crate) fn write_sse_event(
    stream: &mut std::net::TcpStream,
    event: &str,
    data: &Value,
) -> std::io::Result<()> {
    use std::io::Write as _;
    let s = serde_json::to_string(data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    write!(stream, "event: {event}\ndata: {s}\n\n")?;
    stream.flush()
}

pub(crate) fn with_meta_policy(
    res: Result<Value, String>,
    cfg: &UiConfig,
) -> Result<Value, String> {
    res.map(|mut val| {
        if let Value::Object(ref mut map) = val {
            map.insert("read_only".into(), Value::Bool(!cfg.allow_mutating));
            map.insert(
                "allow_approve".into(),
                Value::Bool(cfg.effective_allow_approve()),
            );
        }
        val
    })
}

pub(crate) fn wrap_ok(val: Value) -> Value {
    match val {
        Value::Object(mut map) => {
            map.entry("ok".to_string()).or_insert(Value::Bool(true));
            Value::Object(map)
        }
        other => serde_json::json!({ "ok": true, "data": other }),
    }
}

pub(crate) fn write_backend_result(stream: &mut std::net::TcpStream, res: Result<Value, String>) {
    match res {
        Ok(val) => {
            let _ = write_json_response(stream, 200, &wrap_ok(val));
        }
        Err(msg) => {
            let lower = msg.to_ascii_lowercase();
            let code = if lower.contains("not found") {
                404
            } else if lower.starts_with("refused:") {
                403
            } else if lower.starts_with("invalid") || lower.contains("mismatch") {
                400
            } else {
                500
            };
            let _ = write_json_error(stream, code, &msg);
        }
    }
}

fn status_reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    }
}

pub(crate) fn write_json_error(
    stream: &mut std::net::TcpStream,
    code: u16,
    msg: &str,
) -> std::io::Result<()> {
    write_json_response(
        stream,
        code,
        &serde_json::json!({ "ok": false, "error": msg }),
    )
}

pub(crate) fn write_json_response(
    stream: &mut std::net::TcpStream,
    code: u16,
    body: &Value,
) -> std::io::Result<()> {
    let payload = serde_json::to_vec(body)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    write_http_response(
        stream,
        code,
        "application/json; charset=utf-8",
        "",
        &payload,
    )
}

pub(crate) fn write_http_response(
    stream: &mut std::net::TcpStream,
    code: u16,
    content_type: &str,
    extra_headers: &str,
    body: &[u8],
) -> std::io::Result<()> {
    use std::io::Write as _;
    let header = format!(
        "HTTP/1.1 {code} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         {extra_headers}X-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\n\
         Referrer-Policy: no-referrer\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        status_reason(code),
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}
