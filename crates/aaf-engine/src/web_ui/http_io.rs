//! HTTP response/query formatting helpers for `web_ui`.

use serde_json::Value;

use super::UiConfig;

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

pub(crate) fn backend_error_status(msg: &str) -> u16 {
    let lower = msg.to_ascii_lowercase();
    if lower.starts_with("refused:") {
        403
    } else if lower.starts_with("invalid") || lower.contains("mismatch") {
        400
    } else if lower.starts_with("not found") || lower.contains("not found") {
        404
    } else {
        500
    }
}

pub(crate) fn write_backend_result(stream: &mut std::net::TcpStream, res: Result<Value, String>) {
    match res {
        Ok(val) => {
            let _ = write_json_response(stream, 200, &wrap_ok(val));
        }
        Err(msg) => {
            let _ = write_json_error(stream, backend_error_status(&msg), &msg);
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
