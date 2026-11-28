//! Structured error, usage, and wording classifiers for Claude Code `stream-json`.

use aaf_domain::ports::{InfraFailure, InfraKind};
use serde_json::Value;

use super::RAW_LIMIT_CHARS;

/// Values of structured error fields that mean the provider refused the
/// account itself (permanent until fixed; never retried).
const ACCOUNT_ERRORS: [&str; 7] = [
    "authentication_failed",
    "authentication_error",
    "oauth_org_not_allowed",
    "account_on_hold",
    "verification_required",
    "billing_error",
    "cloud_credential_error",
];

/// Structured error codes for transient provider/server failures (mapped to
/// [`InfraKind::Network`] so `Mode::Auto` can back off and retry).
const TRANSIENT_SERVER_ERRORS: [&str; 3] = ["server_error", "overloaded", "overloaded_error"];

/// How Claude Code 2.1.330 words a request that never reached the API.
/// Matched only in a message the CLI flags as its own API error, never in agent prose.
#[rustfmt::skip]
const NETWORK_WORDING: [&str; 14] = [
    "unable to connect to api", "can't reach the api server", "no internet route",
    "a firewall or proxy may be blocking it", "couldn't connect through your proxy",
    "connection dropped", "connection error", "enotfound", "eai_again", "etimedout",
    "econnrefused", "econnreset", "enetunreach", "ehostunreach",
];

pub(super) fn structured_infra(
    event: &serde_json::Map<String, Value>,
    fallback_text: &str,
) -> Option<InfraFailure> {
    let error_val = event.get("error");
    let error_obj = error_val.and_then(Value::as_object);
    fn str_field<'a>(o: Option<&'a serde_json::Map<String, Value>>, k: &str) -> Option<&'a str> {
        o.and_then(|m| m.get(k)).and_then(Value::as_str)
    }
    let error_status = error_obj
        .and_then(|o| o.get("status").or_else(|| o.get("status_code")))
        .and_then(Value::as_i64);
    let status = event
        .get("api_error_status")
        .or_else(|| event.get("status_code"))
        .and_then(Value::as_i64)
        .or(error_status);
    let codes = [
        error_val.and_then(Value::as_str),
        str_field(error_obj, "type"),
        event.get("subtype").and_then(Value::as_str),
    ];
    let has_code = |list: &[&str]| {
        codes
            .iter()
            .flatten()
            .any(|c| list.iter().any(|known| known.eq_ignore_ascii_case(c)))
    };
    let kind = if has_code(&ACCOUNT_ERRORS) || matches!(status, Some(401 | 403)) {
        InfraKind::Auth
    } else if has_code(&TRANSIENT_SERVER_ERRORS) || matches!(status, Some(500..=599)) {
        InfraKind::Network
    } else {
        return None;
    };
    let detail = [fallback_text, str_field(error_obj, "message").unwrap_or("")]
        .into_iter()
        .chain(codes.into_iter().flatten())
        .find(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| status.map(|s| format!("HTTP {s}")))
        .unwrap_or_default();
    Some(InfraFailure {
        kind,
        raw: detail.chars().take(RAW_LIMIT_CHARS).collect(),
    })
}

pub(super) fn network_wording_infra(flagged: bool, text: &str) -> Option<InfraFailure> {
    let lower = text.to_lowercase();
    (flagged && NETWORK_WORDING.iter().any(|words| lower.contains(words))).then(|| InfraFailure {
        kind: InfraKind::Network,
        raw: text.chars().take(RAW_LIMIT_CHARS).collect(),
    })
}

/// `(input + cache creation, output)` of a `usage` object.
pub(super) fn usage_tokens(usage: &Value) -> (u64, u64) {
    let field = |name: &str| usage.get(name).and_then(Value::as_u64).unwrap_or(0);
    (
        field("input_tokens").saturating_add(field("cache_creation_input_tokens")),
        field("output_tokens"),
    )
}

/// `(turn_context_tokens, cache_read_input_tokens)` of a `usage` object.
pub(super) fn context_tokens(usage: &Value) -> (u64, u64) {
    let field = |name: &str| usage.get(name).and_then(Value::as_u64).unwrap_or(0);
    let cache_read = field("cache_read_input_tokens");
    let turn_ctx = field("input_tokens")
        .saturating_add(field("cache_creation_input_tokens"))
        .saturating_add(cache_read);
    (turn_ctx, cache_read)
}

pub(super) fn message_text(message: &Value) -> String {
    message
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

pub(super) fn is_limit_wording(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("usage limit reached")
        || lower.contains("out of usage credits")
        || (lower.contains("hit your") && lower.contains("limit"))
}

/// Older releases appended the reset time as `...|<unix seconds>`.
pub(super) fn reset_from_text(text: &str) -> Option<i64> {
    let (_, tail) = text.rsplit_once('|')?;
    let digits: String = tail
        .trim()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    (digits.len() == 10).then(|| digits.parse().ok()).flatten()
}
