//! Incremental parser and context-occupancy telemetry for Claude Code
//! `--output-format stream-json` transcripts.

use std::collections::HashMap;

use aaf_domain::ports::{InfraFailure, InfraKind, LimitHit};
use serde_json::Value;

pub(super) const RAW_LIMIT_CHARS: usize = 500;

/// What one `stream-json` transcript amounts to.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StreamSummary {
    /// Text of the `result` event, or of the last assistant message when the
    /// run ended before one was written.
    pub final_text: String,
    /// Billed input + cache-creation tokens; cache reads are NEVER added here.
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// Highest single-turn context occupancy (`input + cache_creation + cache_read`).
    pub peak_context_tokens: u64,
    /// Total `cache_read_input_tokens` observed across the run (advisory telemetry).
    pub cache_read_tokens: u64,
    /// Model identifier reported by the stream (`message.model` or `system.model`).
    pub observed_model: Option<String>,
    pub limit_hit: Option<LimitHit>,
    /// The run failed because of the account or the network.
    pub infra: Option<InfraFailure>,
    /// The `result` event reported an error.
    pub is_error: bool,
    pub saw_result: bool,
}

/// Resolves `(context_window_tokens, window_inferred)` for a reported Claude model id.
///
/// Explicit `[1m]` suffixes return `(1_000_000, false)`. Standard Claude 3/4/5
/// families without `[1m]` return `(200_000, true)`. Unknown models return `None`.
pub fn resolve_context_window(model: Option<&str>) -> Option<(u64, bool)> {
    let m = model?.trim().to_ascii_lowercase();
    if m.is_empty() {
        return None;
    }
    if m.contains("[1m]") {
        return Some((1_000_000, false));
    }
    if m.starts_with("claude-opus")
        || m.starts_with("claude-sonnet")
        || m.starts_with("claude-haiku")
    {
        return Some((200_000, true));
    }
    None
}

/// Incremental parser for `claude --output-format stream-json`.
///
/// Every line is untrusted: anything that is not a JSON object of a known
/// type is skipped (the raw text stays in the run's log tail).
#[derive(Debug, Default)]
pub struct StreamParser {
    /// Billed usage per assistant message id. One message is streamed as several
    /// events repeating the same usage, so the last one per id wins.
    assistant_usage: HashMap<String, (u64, u64)>,
    /// Context occupancy `(turn_ctx, cache_read)` per assistant message id.
    assistant_ctx: HashMap<String, (u64, u64)>,
    observed_model: Option<String>,
    last_assistant_text: String,
    result: Option<ResultEvent>,
    /// An assistant message flagged `"error":"rate_limit"`.
    rate_limit_error: Option<String>,
    /// The last `rate_limit_event` whose status was `rejected`.
    rejected: Option<(Option<i64>, String)>,
    /// What the last API error message of the CLI says about the account
    /// or the network; `None` when that error was of another kind.
    api_error: Option<InfraFailure>,
}

#[derive(Debug)]
struct ResultEvent {
    text: String,
    is_error: bool,
    /// True when the CLI exited due to a local execution ceiling (e.g.
    /// `error_max_turns`, `error_max_budget_usd`, `error_max_structured_output_retries`)
    /// without a structured API/HTTP error: must stay fail-closed (`is_error = true`)
    /// but must never be misclassified as `LimitHit` or `InfraFailure`.
    is_local_cap_error: bool,
    api_error_status: Option<i64>,
    infra: Option<InfraFailure>,
    tokens: (u64, u64),
    ctx: (u64, u64),
}

fn is_error_subtype(subtype: Option<&str>) -> bool {
    subtype.is_some_and(|s| {
        s.as_bytes()
            .get(..6)
            .is_some_and(|p| p.eq_ignore_ascii_case(b"error_"))
    })
}

fn is_local_cap_subtype(subtype: Option<&str>) -> bool {
    subtype.is_some_and(|s| {
        s.eq_ignore_ascii_case("error_max_turns")
            || s.eq_ignore_ascii_case("error_max_budget_usd")
            || s.eq_ignore_ascii_case("error_max_structured_output_retries")
    })
}

impl StreamParser {
    pub fn feed_line(&mut self, line: &str) {
        let Ok(Value::Object(event)) = serde_json::from_str::<Value>(line.trim()) else {
            return;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("system") => {
                if let Some(model) = event
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|m| !m.is_empty())
                {
                    self.observed_model = Some(model.to_string());
                }
            }
            Some("assistant") => {
                let message = event.get("message");
                if let Some(model) = message
                    .and_then(|m| m.get("model"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|m| !m.is_empty())
                {
                    self.observed_model = Some(model.to_string());
                }
                let text = message.map(message_text).unwrap_or_default();
                let error = event.get("error").and_then(Value::as_str);
                if error == Some("rate_limit") {
                    self.rate_limit_error = Some(text.clone());
                }
                let flagged =
                    event.get("is_api_error_message").and_then(Value::as_bool) == Some(true);
                let has_error = event.get("error").is_some_and(|e| !e.is_null());
                let is_synthetic = message.and_then(|m| m.get("model")).and_then(Value::as_str)
                    == Some("<synthetic>");
                let has_content = !text.is_empty()
                    || message
                        .and_then(|m| m.get("content"))
                        .and_then(Value::as_array)
                        .is_some_and(|blocks| !blocks.is_empty());
                let structured = structured_infra(&event, &text);
                if structured.is_some() {
                    self.api_error = structured;
                } else if has_error || flagged {
                    self.api_error =
                        network_wording_infra(flagged, &text).or_else(|| auth_wording_infra(&text));
                } else if !is_synthetic && has_content {
                    // A subsequent clean non-synthetic assistant turn proves any
                    // earlier transient API/rate-limit error recovered.
                    self.api_error = None;
                    self.rate_limit_error = None;
                }
                if let Some(usage) = message.and_then(|m| m.get("usage")) {
                    let id = message
                        .and_then(|m| m.get("id"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("#{}", self.assistant_usage.len()));
                    self.assistant_usage.insert(id.clone(), usage_tokens(usage));
                    self.assistant_ctx.insert(id, context_tokens(usage));
                }
                if !text.is_empty() {
                    self.last_assistant_text = text;
                }
            }
            Some("error") => {
                let msg = event
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if let Some(failure) = structured_infra(&event, msg) {
                    self.api_error = Some(failure);
                }
            }
            Some("result") => {
                let raw_result = event
                    .get("result")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let text = event
                    .get("structured_output")
                    .filter(|v| !v.is_null())
                    .and_then(|v| serde_json::to_string(v).ok())
                    .unwrap_or_else(|| raw_result.to_string());
                let subtype = event.get("subtype").and_then(Value::as_str);
                let is_error = event
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                    || is_error_subtype(subtype);
                let api_error_status = event
                    .get("api_error_status")
                    .or_else(|| event.get("status_code"))
                    .and_then(Value::as_i64);
                let api_err = is_error
                    && event.get("terminal_reason").and_then(Value::as_str) == Some("api_error");
                let tokens = event.get("usage").map(usage_tokens).unwrap_or_default();
                let ctx = event.get("usage").map(context_tokens).unwrap_or_default();
                let structured = structured_infra(&event, raw_result);
                let is_local_cap_error = is_local_cap_subtype(subtype)
                    && !api_err
                    && api_error_status.is_none()
                    && structured.is_none();
                let infra = structured
                    .or_else(|| network_wording_infra(api_err, raw_result))
                    .or_else(|| {
                        (!is_local_cap_error && (is_error || tokens == (0, 0)))
                            .then(|| auth_wording_infra(raw_result))
                            .flatten()
                    });
                self.result = Some(ResultEvent {
                    text,
                    is_error,
                    is_local_cap_error,
                    api_error_status,
                    infra,
                    tokens,
                    ctx,
                });
            }
            Some("rate_limit_event") => {
                let info = event.get("rate_limit_info");
                let status = info.and_then(|i| i.get("status")).and_then(Value::as_str);
                if status == Some("rejected") {
                    let resets_at = info.and_then(|i| i.get("resetsAt")).and_then(Value::as_i64);
                    self.rejected = Some((resets_at, line.trim().to_string()));
                }
            }
            _ => {}
        }
    }

    pub fn summary(&self) -> StreamSummary {
        // The result event carries the totals of the whole run. Without one
        // (timeout, crash) the per-message numbers are all there is.
        let (tokens_in, tokens_out) = match &self.result {
            Some(result) => result.tokens,
            None => self
                .assistant_usage
                .values()
                .fold((0u64, 0u64), |acc, usage| {
                    (acc.0.saturating_add(usage.0), acc.1.saturating_add(usage.1))
                }),
        };
        let peak_context_tokens = self
            .assistant_ctx
            .values()
            .map(|&(turn_ctx, _)| turn_ctx)
            .max()
            .unwrap_or_else(|| self.result.as_ref().map_or(0, |r| r.ctx.0));
        let assistant_cache_read = self
            .assistant_ctx
            .values()
            .fold(0u64, |acc, &(_, cr)| acc.saturating_add(cr));
        let cache_read_tokens = match &self.result {
            Some(result) if result.ctx.1 > 0 => result.ctx.1,
            _ => assistant_cache_read,
        };
        let final_text = match &self.result {
            Some(result) => result.text.clone(),
            None => self.last_assistant_text.clone(),
        };
        StreamSummary {
            final_text,
            tokens_in,
            tokens_out,
            peak_context_tokens,
            cache_read_tokens,
            observed_model: self.observed_model.clone(),
            limit_hit: self.limit_hit(),
            infra: self.infra(),
            is_error: self.result.as_ref().is_some_and(|r| r.is_error),
            saw_result: self.result.is_some(),
        }
    }

    fn limit_hit(&self) -> Option<LimitHit> {
        // A run that produced a successful result was served, whatever the
        // rate-limit telemetry said on the way (a window can report
        // `rejected` while paid overage still covers the request).
        if self
            .result
            .as_ref()
            .is_some_and(|r| !r.is_error || r.is_local_cap_error)
        {
            return None;
        }
        let failed_text = self
            .result
            .as_ref()
            .map(|r| r.text.as_str())
            .unwrap_or(&self.last_assistant_text);
        let message = if let Some(text) = &self.rate_limit_error {
            text.clone()
        } else if self.result.as_ref().and_then(|r| r.api_error_status) == Some(429)
            || is_limit_wording(failed_text)
        {
            failed_text.to_string()
        } else if let Some((_, raw)) = &self.rejected {
            raw.clone()
        } else {
            return None;
        };
        let reset_at = self
            .rejected
            .as_ref()
            .and_then(|(resets_at, _)| *resets_at)
            .or_else(|| reset_from_text(&message));
        Some(LimitHit {
            reset_at,
            raw: message.chars().take(RAW_LIMIT_CHARS).collect(),
        })
    }

    /// A failed run whose last API error was about the account, the network,
    /// or a structured transient server error (HTTP 5xx / `overloaded` /
    /// `server_error`), or whose result carries HTTP 401/403/5xx. A run that
    /// was served in the end, or that hit a limit (handled as a limit), is none.
    fn infra(&self) -> Option<InfraFailure> {
        if self.limit_hit().is_some() {
            return None;
        }
        if let Some(res) = &self.result
            && res.is_local_cap_error
        {
            return None;
        }
        if let Some(res) = &self.result
            && !res.is_error
        {
            // Claude CLI can emit subtype="success" (is_error=false) with 0/0
            // tokens when OAuth Keychain refresh fails before reaching the API.
            if res.tokens == (0, 0)
                && res
                    .infra
                    .as_ref()
                    .is_some_and(|i| i.kind == InfraKind::Auth)
            {
                return res.infra.clone();
            }
            return None;
        }
        if let Some(failure) = &self.api_error {
            return Some(failure.clone());
        }
        if let Some(res) = &self.result {
            return res.infra.clone();
        }
        let zero_tokens = self
            .assistant_usage
            .values()
            .all(|&(tok_in, tok_out)| tok_in == 0 && tok_out == 0);
        zero_tokens
            .then(|| auth_wording_infra(&self.last_assistant_text))
            .flatten()
    }
}

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

/// How Claude Code 2.1.330 words a request that never reached the API
/// (read from the binary's connection-error messages; never observed
/// live). Matched only in a message the CLI flags as its own API error,
/// never in the agent's words.
#[rustfmt::skip]
const NETWORK_WORDING: [&str; 13] = [
    "unable to connect to api", "can't reach the api server", "no internet route",
    "a firewall or proxy may be blocking it", "couldn't connect through your proxy",
    "connection dropped", "enotfound", "eai_again", "etimedout", "econnrefused",
    "econnreset", "enetunreach", "ehostunreach",
];

fn structured_infra(
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

/// Wording Claude Code CLI emits when OAuth token refresh or macOS Keychain
/// persistence fails (delegated to `claude_flags`).
pub(super) fn auth_wording_infra(text: &str) -> Option<InfraFailure> {
    super::super::claude_flags::auth_wording_infra(text, RAW_LIMIT_CHARS)
}

fn network_wording_infra(flagged: bool, text: &str) -> Option<InfraFailure> {
    let lower = text.to_lowercase();
    (flagged && NETWORK_WORDING.iter().any(|words| lower.contains(words))).then(|| InfraFailure {
        kind: InfraKind::Network,
        raw: text.chars().take(RAW_LIMIT_CHARS).collect(),
    })
}

/// Parses a whole transcript.
pub fn parse_stream(text: &str) -> StreamSummary {
    let mut parser = StreamParser::default();
    for line in text.lines() {
        parser.feed_line(line);
    }
    parser.summary()
}

/// `(input + cache creation, output)` of a `usage` object.
fn usage_tokens(usage: &Value) -> (u64, u64) {
    let field = |name: &str| usage.get(name).and_then(Value::as_u64).unwrap_or(0);
    (
        field("input_tokens").saturating_add(field("cache_creation_input_tokens")),
        field("output_tokens"),
    )
}

/// `(turn_context_tokens, cache_read_input_tokens)` of a `usage` object.
fn context_tokens(usage: &Value) -> (u64, u64) {
    let field = |name: &str| usage.get(name).and_then(Value::as_u64).unwrap_or(0);
    let cache_read = field("cache_read_input_tokens");
    let turn_ctx = field("input_tokens")
        .saturating_add(field("cache_creation_input_tokens"))
        .saturating_add(cache_read);
    (turn_ctx, cache_read)
}

fn message_text(message: &Value) -> String {
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

/// Wording Claude Code 2.1.330 uses for exhausted usage (read from the
/// binary's message table; never observed live).
fn is_limit_wording(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("usage limit reached")
        || lower.contains("out of usage credits")
        || (lower.contains("hit your") && lower.contains("limit"))
}

/// Older releases appended the reset time as `...|<unix seconds>`.
fn reset_from_text(text: &str) -> Option<i64> {
    let (_, tail) = text.rsplit_once('|')?;
    let digits: String = tail
        .trim()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    // Ten digits: a plausible Unix time in seconds, not some other number.
    (digits.len() == 10).then(|| digits.parse().ok()).flatten()
}
