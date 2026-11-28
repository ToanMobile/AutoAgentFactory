//! JSON & ANSI output parsing for Antigravity CLI (`agy --output-format json`).

use aaf_domain::ports::{InfraFailure, LimitHit};
use serde_json::{Map, Value};

use super::classify::classify_text_failure;

/// Parsed result of `agy --output-format json`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgyParsed {
    pub final_text: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub usage_present: bool,
    pub is_error: bool,
    pub is_local_cap_error: bool,
    pub limit_hit: Option<LimitHit>,
    pub infra: Option<InfraFailure>,
}

/// Parses `agy --output-format json` stdout (tolerating ANSI escapes and log lines).
pub fn parse_agy_output(stdout: &str, expect_structured: bool) -> AgyParsed {
    let cleaned = strip_ansi(stdout);
    let Some(obj) = extract_json_object(&cleaned) else {
        let trimmed = cleaned.trim().to_string();
        let (limit_hit, infra) = classify_text_failure(None, &trimmed);
        return AgyParsed {
            final_text: trimmed,
            is_error: limit_hit.is_some() || infra.is_some(),
            limit_hit,
            infra,
            ..AgyParsed::default()
        };
    };

    let structured = obj
        .get("structured_output")
        .filter(|v| !v.is_null())
        .and_then(|v| serde_json::to_string(v).ok());
    let response_text = ["response", "result", "text"]
        .into_iter()
        .find_map(|k| obj.get(k).and_then(Value::as_str))
        .unwrap_or_default()
        .to_string();
    let final_text = if expect_structured {
        structured.unwrap_or_else(|| response_text.clone())
    } else if !response_text.is_empty() {
        response_text.clone()
    } else {
        structured.unwrap_or_default()
    };

    let (tokens_in, tokens_out, usage_present) = extract_usage(&obj);
    let subtype = obj.get("subtype").and_then(Value::as_str);
    let is_local_cap = subtype.is_some_and(|s| {
        s.eq_ignore_ascii_case("error_max_turns")
            || s.eq_ignore_ascii_case("error_max_budget_usd")
            || s.eq_ignore_ascii_case("error_max_structured_output_retries")
    });
    let has_non_empty_error = obj.get("error").is_some_and(|e| match e {
        Value::Null | Value::Bool(false) => false,
        Value::String(s) => !s.trim().is_empty(),
        _ => true,
    });
    let (status, msg) = extract_error_details(&obj, &response_text);
    let is_error = obj
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || has_non_empty_error
        || matches!(status, Some(400..=599))
        || obj
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|s| s.eq_ignore_ascii_case("error"))
        || subtype.is_some_and(|s| {
            s.as_bytes()
                .get(..6)
                .is_some_and(|p| p.eq_ignore_ascii_case(b"error_"))
        });
    let is_local_cap_error = is_local_cap && status.is_none() && !has_non_empty_error;
    let (limit_hit, infra) = if is_error && !is_local_cap_error {
        classify_text_failure(status, &msg)
    } else {
        (None, None)
    };
    AgyParsed {
        final_text,
        tokens_in,
        tokens_out,
        usage_present,
        is_error,
        is_local_cap_error,
        limit_hit,
        infra,
    }
}

fn first_u64(map: &Map<String, Value>, keys: &[&str]) -> u64 {
    keys.iter()
        .find_map(|k| map.get(*k).and_then(Value::as_u64))
        .unwrap_or(0)
}

fn extract_usage(obj: &Map<String, Value>) -> (u64, u64, bool) {
    if let Some(u) = obj.get("usage").and_then(Value::as_object) {
        let input = first_u64(u, &["input_tokens"]);
        let cache = first_u64(u, &["cache_read_tokens", "cache_read_input_tokens"]);
        return (
            input.saturating_sub(cache),
            first_u64(u, &["output_tokens"]),
            true,
        );
    }
    let Some(models) = obj
        .get("stats")
        .and_then(|s| s.get("models"))
        .and_then(Value::as_object)
    else {
        return (0, 0, false);
    };
    let (mut tin, mut tout, mut found) = (0u64, 0u64, false);
    for t in models
        .values()
        .filter_map(|e| e.get("tokens").and_then(Value::as_object))
    {
        found = true;
        let input = first_u64(t, &["input", "prompt"]);
        tin = tin.saturating_add(input.saturating_sub(first_u64(t, &["cached"])));
        tout = tout.saturating_add(first_u64(t, &["output", "candidates"]));
    }
    (tin, tout, found)
}

fn extract_error_details(obj: &Map<String, Value>, fallback: &str) -> (Option<i64>, String) {
    let err_val = obj.get("error");
    let err_obj = err_val.and_then(Value::as_object);
    let status = ["status_code", "status"]
        .into_iter()
        .find_map(|k| obj.get(k).and_then(Value::as_i64))
        .or_else(|| {
            err_obj.and_then(|o| {
                ["status_code", "code"]
                    .into_iter()
                    .find_map(|k| o.get(k).and_then(Value::as_i64))
            })
        });
    let mut parts: Vec<&str> = err_val.and_then(Value::as_str).into_iter().collect();
    if let Some(o) = err_obj {
        for key in ["status", "type", "code", "message"] {
            parts.extend(o.get(key).and_then(Value::as_str));
        }
    }
    parts.extend(obj.get("message").and_then(Value::as_str));
    if !fallback.is_empty() {
        parts.push(fallback);
    }
    let detail = if parts.is_empty() {
        status.map(|s| format!("HTTP {s}")).unwrap_or_default()
    } else {
        parts.join(" ")
    };
    (status, detail)
}

pub(super) fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.next_if_eq(&'[').is_some() {
                let _ = chars.by_ref().find(|n| ('@'..='~').contains(n));
            } else if chars.next_if_eq(&']').is_some() {
                while let Some(n) = chars.next() {
                    if n == '\x07' || (n == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
        } else if c != '\r' {
            out.push(c);
        }
    }
    out
}

fn is_payload_json_object(map: &Map<String, Value>) -> bool {
    const PAYLOAD_KEYS: &[&str] = &[
        "response",
        "result",
        "text",
        "structured_output",
        "usage",
        "stats",
        "type",
        "message",
    ];
    PAYLOAD_KEYS.iter().any(|k| map.contains_key(*k))
}

fn is_agy_envelope_object(map: &Map<String, Value>) -> bool {
    const ENVELOPE_KEYS: &[&str] = &[
        "response",
        "result",
        "text",
        "structured_output",
        "usage",
        "stats",
        "status",
        "status_code",
        "error",
        "is_error",
        "subtype",
    ];
    ENVELOPE_KEYS.iter().any(|k| map.contains_key(*k))
}

/// Strips ANSI sequences and removes the byte spans of top-level payload JSON
/// objects (including pretty-printed multi-line JSON), leaving raw non-JSON
/// stderr / CLI wrapper lines and standalone HTTP error JSON fragments intact
/// so agent `"response"` prose never leaks into auth or failure classifiers.
pub(crate) fn strip_ansi_and_json_object(raw: &str) -> String {
    let cleaned = strip_ansi(raw);
    let spans: Vec<_> = extract_json_object_spans(&cleaned)
        .into_iter()
        .filter(|(map, _, _)| {
            let is_pure_http_err = !map.contains_key("response")
                && !map.contains_key("result")
                && !map.contains_key("text")
                && !map.contains_key("structured_output")
                && !map.contains_key("usage")
                && !map.contains_key("stats")
                && !map.contains_key("type")
                && (map.contains_key("status")
                    || map.contains_key("status_code")
                    || map.contains_key("error"));
            !is_pure_http_err && is_payload_json_object(map)
        })
        .collect();
    if spans.is_empty() {
        return cleaned;
    }
    let mut out = String::with_capacity(cleaned.len());
    let mut cursor = 0usize;
    for (_, start, end) in spans {
        if start > cursor {
            if !out.is_empty() && !out.ends_with('\n') && !cleaned[cursor..start].starts_with('\n')
            {
                out.push('\n');
            }
            out.push_str(&cleaned[cursor..start]);
        }
        cursor = end;
    }
    if cursor < cleaned.len() {
        if !out.is_empty() && !out.ends_with('\n') && !cleaned[cursor..].starts_with('\n') {
            out.push('\n');
        }
        out.push_str(&cleaned[cursor..]);
    }
    out
}

fn extract_json_object(cleaned: &str) -> Option<Map<String, Value>> {
    let spans = extract_json_object_spans(cleaned);
    spans
        .iter()
        .rev()
        .find(|(map, _, _)| is_agy_envelope_object(map))
        .or_else(|| spans.last())
        .map(|(map, _, _)| map.clone())
}

fn extract_json_object_spans(cleaned: &str) -> Vec<(Map<String, Value>, usize, usize)> {
    let lead = cleaned.len() - cleaned.trim_start().len();
    let trimmed = cleaned.trim();
    if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(trimmed) {
        return vec![(map, lead, lead + trimmed.len())];
    }
    let (mut spans, mut skip_until) = (Vec::new(), 0usize);
    for (idx, ch) in cleaned.char_indices() {
        if idx < skip_until || ch != '{' {
            continue;
        }
        let mut stream = serde_json::Deserializer::from_str(&cleaned[idx..]).into_iter::<Value>();
        if let Some(Ok(Value::Object(map))) = stream.next() {
            let end = idx.saturating_add(stream.byte_offset());
            skip_until = end;
            spans.push((map, idx, end));
        }
    }
    spans
}
