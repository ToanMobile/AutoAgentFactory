//! Incremental parser and context-occupancy telemetry for Claude Code
//! `--output-format stream-json` transcripts.

use std::collections::{HashMap, HashSet};

use aaf_domain::ports::{InfraFailure, InfraKind, LimitHit};
use serde_json::Value;

#[path = "stream_classify.rs"]
mod classify;
use classify::{
    context_tokens, is_limit_wording, message_text, network_wording_infra, reset_from_text,
    structured_infra, usage_tokens,
};

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
    /// Billed usage of assistant messages not yet rolled into a non-zero
    /// `result` event; cleared whenever a `result` event reports billed tokens.
    unaccounted_assistant_usage: HashMap<String, (u64, u64)>,
    /// Context occupancy `(turn_ctx, cache_read)` per assistant message id.
    assistant_ctx: HashMap<String, (u64, u64)>,
    /// `cache_read` of assistant messages not yet rolled into a `result` event
    /// carrying `cache_read > 0`.
    unaccounted_cache_read: HashMap<String, u64>,
    /// Assistant message ids already settled into `result_tokens_sum`, so late
    /// or replayed chunks for the same id are never double-counted.
    settled_assistant_ids: HashSet<String>,
    /// Assistant message ids whose `cache_read` was already settled into
    /// `result_cache_read_sum`.
    settled_cache_read_ids: HashSet<String>,
    /// Cumulative billed `(tokens_in, tokens_out)` across all `result` events.
    /// Claude Code 2.1.x with `Monitor` emits one per-segment `result` per
    /// wakeup (and sometimes a trailing `result` without `usage`), so summing
    /// prevents later segments or empty results from wiping prior token spend.
    result_tokens_sum: (u64, u64),
    /// Cumulative `cache_read_input_tokens` across `result` events that report it.
    result_cache_read_sum: u64,
    /// Peak context occupancy reported directly by any `result` event.
    result_peak_ctx: u64,
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
    /// Combined `result` + `errors[]` text used for `limit_hit()` and infra
    /// classification when `errors[]` carries details not present in `result`.
    error_text: String,
    is_error: bool,
    /// True when the CLI exited due to a local execution ceiling (e.g.
    /// `error_max_turns`, `error_max_budget_usd`, `error_max_structured_output_retries`)
    /// without a structured API/HTTP error: must stay fail-closed (`is_error = true`)
    /// but must never be misclassified as `LimitHit` or `InfraFailure`.
    is_local_cap_error: bool,
    api_error_status: Option<i64>,
    infra: Option<InfraFailure>,
    tokens: (u64, u64),
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
                    let tok = usage_tokens(usage);
                    let ctx = context_tokens(usage);
                    self.assistant_usage.insert(id.clone(), tok);
                    self.assistant_ctx.insert(id.clone(), ctx);
                    if !self.settled_assistant_ids.contains(&id) {
                        self.unaccounted_assistant_usage.insert(id.clone(), tok);
                    }
                    if !self.settled_cache_read_ids.contains(&id) {
                        self.unaccounted_cache_read.insert(id, ctx.1);
                    }
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
                let errors_joined = event
                    .get("errors")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .filter(|s| !s.trim().is_empty())
                            .collect::<Vec<_>>()
                            .join("; ")
                    })
                    .unwrap_or_default();
                let subtype = event.get("subtype").and_then(Value::as_str);
                let is_error = event
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                    || is_error_subtype(subtype);
                let combined_error;
                let error_text = if !raw_result.is_empty() && is_error && !errors_joined.is_empty()
                {
                    combined_error = format!("{raw_result}; {errors_joined}");
                    combined_error.as_str()
                } else if !raw_result.is_empty() {
                    raw_result
                } else {
                    errors_joined.as_str()
                };
                let text = event
                    .get("structured_output")
                    .filter(|v| !v.is_null())
                    .and_then(|v| serde_json::to_string(v).ok())
                    .unwrap_or_else(|| {
                        if raw_result.is_empty() && is_error {
                            errors_joined.clone()
                        } else {
                            raw_result.to_string()
                        }
                    });
                let api_error_status = event
                    .get("api_error_status")
                    .or_else(|| event.get("status_code"))
                    .and_then(Value::as_i64);
                let api_err = is_error
                    && (event.get("terminal_reason").and_then(Value::as_str) == Some("api_error")
                        || (subtype == Some("error_during_execution") && !error_text.is_empty()));
                let tokens = event.get("usage").map(usage_tokens).unwrap_or_default();
                let ctx = event.get("usage").map(context_tokens).unwrap_or_default();
                if tokens.0 > 0 || tokens.1 > 0 {
                    self.result_tokens_sum.0 = self.result_tokens_sum.0.saturating_add(tokens.0);
                    self.result_tokens_sum.1 = self.result_tokens_sum.1.saturating_add(tokens.1);
                    self.settled_assistant_ids
                        .extend(self.unaccounted_assistant_usage.drain().map(|(k, _)| k));
                    if ctx.1 > 0 {
                        self.result_cache_read_sum =
                            self.result_cache_read_sum.saturating_add(ctx.1);
                        self.settled_cache_read_ids
                            .extend(self.unaccounted_cache_read.drain().map(|(k, _)| k));
                    } else {
                        let mut seg_cr = 0u64;
                        for (id, cr) in self.unaccounted_cache_read.drain() {
                            seg_cr = seg_cr.saturating_add(cr);
                            self.settled_cache_read_ids.insert(id);
                        }
                        self.result_cache_read_sum =
                            self.result_cache_read_sum.saturating_add(seg_cr);
                    }
                } else if ctx.1 > 0 {
                    self.result_cache_read_sum = self.result_cache_read_sum.saturating_add(ctx.1);
                    self.settled_cache_read_ids
                        .extend(self.unaccounted_cache_read.drain().map(|(k, _)| k));
                }
                self.result_peak_ctx = self.result_peak_ctx.max(ctx.0);
                let run_zero_tokens = self.run_has_zero_tokens();
                let structured = structured_infra(&event, error_text);
                let is_local_cap_error = is_local_cap_subtype(subtype)
                    && !api_err
                    && api_error_status.is_none()
                    && structured.is_none();
                let infra = structured
                    .or_else(|| network_wording_infra(api_err, error_text))
                    .or_else(|| {
                        (!is_local_cap_error && (is_error || run_zero_tokens))
                            .then(|| auth_wording_infra(error_text))
                            .flatten()
                    });
                let incoming = ResultEvent {
                    text,
                    error_text: error_text.to_string(),
                    is_error,
                    is_local_cap_error,
                    api_error_status,
                    infra,
                    tokens,
                };
                self.merge_result(incoming);
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
        // Each non-zero `result` event settles its segment and clears
        // `unaccounted_assistant_usage`; adding any remaining assistant usage
        // preserves totals across multi-result `Monitor` runs, trailing empty
        // `result` events, and mid-turn timeouts/crashes.
        let (unacc_in, unacc_out) = self
            .unaccounted_assistant_usage
            .values()
            .fold((0u64, 0u64), |acc, usage| {
                (acc.0.saturating_add(usage.0), acc.1.saturating_add(usage.1))
            });
        let tokens_in = self.result_tokens_sum.0.saturating_add(unacc_in);
        let tokens_out = self.result_tokens_sum.1.saturating_add(unacc_out);
        let peak_context_tokens = self
            .assistant_ctx
            .values()
            .map(|&(turn_ctx, _)| turn_ctx)
            .max()
            .filter(|&v| v > 0)
            .unwrap_or(self.result_peak_ctx);
        let unacc_cache_read = self
            .unaccounted_cache_read
            .values()
            .fold(0u64, |acc, &cr| acc.saturating_add(cr));
        let cache_read_tokens = self.result_cache_read_sum.saturating_add(unacc_cache_read);
        let final_text = match &self.result {
            Some(result) if !result.text.is_empty() => result.text.clone(),
            _ => self.last_assistant_text.clone(),
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
            .map(|r| {
                if !r.error_text.is_empty() {
                    r.error_text.as_str()
                } else {
                    r.text.as_str()
                }
            })
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
            // run-wide tokens when OAuth Keychain refresh fails before reaching the API.
            if self.run_has_zero_tokens()
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
        let zero_tokens = self.run_has_zero_tokens();
        zero_tokens
            .then(|| auth_wording_infra(&self.last_assistant_text))
            .flatten()
    }

    fn run_has_zero_tokens(&self) -> bool {
        self.result_tokens_sum == (0, 0)
            && self
                .assistant_usage
                .values()
                .all(|&(tok_in, tok_out)| tok_in == 0 && tok_out == 0)
    }

    fn merge_result(&mut self, incoming: ResultEvent) {
        let is_empty_pointer = !incoming.is_error
            && incoming.text.is_empty()
            && incoming.tokens == (0, 0)
            && incoming.infra.is_none()
            && incoming.api_error_status.is_none();
        if self.result.is_some() && is_empty_pointer {
            return;
        }
        let merged = match self.result.take() {
            None => incoming,
            Some(prev) => {
                let did_work = incoming.tokens != (0, 0) || !incoming.text.is_empty();
                let is_error = incoming.is_error || (prev.is_error && !did_work);
                let is_local_cap_error = if incoming.is_error {
                    incoming.is_local_cap_error
                } else if prev.is_error && !did_work {
                    prev.is_local_cap_error
                } else {
                    false
                };
                ResultEvent {
                    text: if !incoming.text.is_empty() {
                        incoming.text
                    } else {
                        prev.text
                    },
                    error_text: if !incoming.error_text.is_empty() {
                        incoming.error_text
                    } else {
                        prev.error_text
                    },
                    is_error,
                    is_local_cap_error,
                    api_error_status: is_error
                        .then(|| incoming.api_error_status.or(prev.api_error_status))
                        .flatten(),
                    infra: if is_error {
                        incoming.infra.or(prev.infra)
                    } else {
                        incoming.infra
                    },
                    tokens: (
                        prev.tokens.0.saturating_add(incoming.tokens.0),
                        prev.tokens.1.saturating_add(incoming.tokens.1),
                    ),
                }
            }
        };
        self.result = Some(merged);
    }
}

/// Wording Claude Code CLI emits when OAuth token refresh or macOS Keychain
/// persistence fails (delegated to `claude_flags`).
pub(super) fn auth_wording_infra(text: &str) -> Option<InfraFailure> {
    super::super::claude_flags::auth_wording_infra(text, RAW_LIMIT_CHARS)
}

/// Parses a whole transcript.
pub fn parse_stream(text: &str) -> StreamSummary {
    let mut parser = StreamParser::default();
    for line in text.lines() {
        parser.feed_line(line);
    }
    parser.summary()
}
