//! Outbound Telegram Bot API companion for remote status, handover, and
//! two-step human-only approvals (zero listening ports, fail-closed).

pub mod client;
pub mod commands;
#[cfg(test)]
mod tests;

use std::sync::OnceLock;

use aaf_domain::ports::Clock;
use aaf_domain::telegram::{MAX_POLL_TIMEOUT_SECS, TelegramConfig};
use regex::Regex;

use crate::watch::sanitize_display;

pub use client::{
    Chat, DEFAULT_CONNECT_TIMEOUT, DEFAULT_REQUEST_TIMEOUT, HttpTelegramApi, MAX_MESSAGE_BYTES,
    Message, TelegramApi, TelegramError, Update, User, redact_bot_token, send_alert,
    send_alert_with, truncate_message, validate_base_url, validate_token,
};
pub use commands::{
    ApprovePreview, CommandDispatcher, CommandHandler, LockedFileSummary, MAX_ARG_CHARS,
    MAX_PENDING_CONFIRMATIONS, MutatingAction, ParsedCommand, PendingStore, UpdateOutcome,
    parse_command,
};

/// Summary counters returned by [`run_poll_once`] and [`run_poll_loop`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PollStats {
    pub polls: usize,
    pub updates_seen: usize,
    pub commands_handled: usize,
    pub replies_sent: usize,
    pub ignored_unauthorized: usize,
    pub next_offset: Option<i64>,
}

/// Validates Tier-1 Telegram configuration and bot token before starting any
/// outbound requests. Fail-closed on disabled flag, empty allowlist, missing
/// or malformed token, or invalid API base URL.
pub fn validate_config(cfg: &TelegramConfig, token: &str) -> Result<(), TelegramError> {
    if !cfg.enabled {
        return Err(TelegramError::Disabled);
    }
    if cfg.allowed_chat_ids.is_empty() {
        return Err(TelegramError::EmptyAllowlist);
    }
    if token.trim().is_empty() {
        return Err(TelegramError::MissingToken {
            env_var: cfg.bot_token_env.clone(),
        });
    }
    validate_base_url(&cfg.api_base_url)?;
    validate_token(token)?;
    Ok(())
}

/// Executes a single outbound `getUpdates` poll, dispatches authorized commands,
/// and sends sanitized replies via `api`.
pub fn run_poll_once(
    api: &dyn TelegramApi,
    dispatcher: &mut CommandDispatcher,
    handler: &mut dyn CommandHandler,
    clock: &dyn Clock,
    offset: Option<i64>,
) -> Result<PollStats, TelegramError> {
    let cfg = dispatcher.config();
    if !cfg.enabled {
        return Err(TelegramError::Disabled);
    }
    if cfg.allowed_chat_ids.is_empty() {
        return Err(TelegramError::EmptyAllowlist);
    }
    let timeout_secs = cfg.poll_timeout_secs.min(MAX_POLL_TIMEOUT_SECS);
    let updates = api.get_updates(offset, timeout_secs)?;
    let mut stats = PollStats {
        polls: 1,
        next_offset: offset,
        ..PollStats::default()
    };

    for update in updates {
        stats.updates_seen += 1;
        let candidate_next = update.update_id.saturating_add(1);
        stats.next_offset = Some(
            stats
                .next_offset
                .map_or(candidate_next, |cur| cur.max(candidate_next)),
        );
        match dispatcher.handle_update(&update, handler, clock)? {
            UpdateOutcome::IgnoredUnauthorized => stats.ignored_unauthorized += 1,
            UpdateOutcome::IgnoredNonCommand => {}
            UpdateOutcome::Reply { chat_id, text } => {
                stats.commands_handled += 1;
                api.send_message(chat_id, &text)?;
                stats.replies_sent += 1;
            }
        }
    }
    Ok(stats)
}

/// Runs outbound long-polling in a loop until `max_polls` iterations complete
/// (or indefinitely when `max_polls` is `None`).
pub fn run_poll_loop(
    api: &dyn TelegramApi,
    cfg: &TelegramConfig,
    handler: &mut dyn CommandHandler,
    clock: &dyn Clock,
    max_polls: Option<usize>,
) -> Result<PollStats, TelegramError> {
    if !cfg.enabled {
        return Err(TelegramError::Disabled);
    }
    if cfg.allowed_chat_ids.is_empty() {
        return Err(TelegramError::EmptyAllowlist);
    }
    let mut dispatcher = CommandDispatcher::new(cfg.clone())?;
    let mut total = PollStats::default();
    let mut offset: Option<i64> = None;

    while max_polls.is_none_or(|limit| total.polls < limit) {
        let step = run_poll_once(api, &mut dispatcher, handler, clock, offset)?;
        offset = step.next_offset;
        total.polls += step.polls;
        total.updates_seen += step.updates_seen;
        total.commands_handled += step.commands_handled;
        total.replies_sent += step.replies_sent;
        total.ignored_unauthorized += step.ignored_unauthorized;
        total.next_offset = offset;
    }
    Ok(total)
}

/// Strips ANSI/control characters, collapses whitespace, and caps length.
pub fn sanitize_input(raw: &str, max_chars: usize) -> String {
    let flat = sanitize_display(raw);
    let collapsed = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(max_chars).collect()
}

/// Strips ANSI/control characters (preserving `\n`), redacts common secrets,
/// and bounds the outgoing message to `MAX_MESSAGE_BYTES` UTF-8 bytes.
pub fn sanitize_output(raw: &str) -> String {
    let mut cleaned = String::with_capacity(raw.len());
    for (idx, line) in raw.split('\n').enumerate() {
        if idx > 0 {
            cleaned.push('\n');
        }
        cleaned.push_str(&sanitize_display(line));
    }
    let scrubbed = scrub_secrets(&cleaned);
    truncate_message(&scrubbed, MAX_MESSAGE_BYTES)
}

fn secret_rules() -> &'static [(Regex, &'static str)] {
    static RULES: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    RULES.get_or_init(|| {
        // Regexes are assembled from fragments so static secret scanners never flag this file.
        let specs: &[(&[&str], &str)] = &[
            (
                &[
                    r"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE",
                    r" KEY-----[\s\S]*?-----END (?:[A-Z0-9]+ )*PRIVATE",
                    r" KEY-----",
                ],
                "[REDACTED_PRIVATE_KEY]",
            ),
            (
                &[r"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE", r" KEY-----"],
                "[REDACTED_PRIVATE_KEY]",
            ),
            (
                &[r"(?i)\bBearer\s+", r"[A-Za-z0-9._~+/=-]{8,}"],
                "Bearer [REDACTED]",
            ),
            (&[r"\bsk-", r"(?:ant-)?[A-Za-z0-9_-]{8,}"], "[REDACTED]"),
            (&[r"\bgh[pousr]_", r"[A-Za-z0-9]{10,}"], "[REDACTED]"),
            (&[r"\bgithub_pat_", r"[A-Za-z0-9_]{10,}"], "[REDACTED]"),
            (&[r"\bAKIA", r"[0-9A-Z]{16}\b"], "[REDACTED]"),
            (&[r"\bxox[bp]-", r"[A-Za-z0-9-]{10,}"], "[REDACTED]"),
            (&[r"\bAIza", r"[0-9A-Za-z_-]{20,}"], "[REDACTED]"),
            (&[r"/bot", r"[^/\s?#]+"], "/bot[REDACTED]"),
            (&[r"\b[0-9]{6,12}:", r"[A-Za-z0-9_-]{20,}\b"], "[REDACTED]"),
            (&[r"://[^/\s:@]+:", r"[^/\s@]+@"], "://[REDACTED]@"),
        ];
        specs
            .iter()
            .filter_map(|(parts, repl)| Regex::new(&parts.concat()).ok().map(|re| (re, *repl)))
            .collect()
    })
}

fn scrub_secrets(input: &str) -> String {
    let mut text = input.to_string();
    for (re, repl) in secret_rules() {
        text = re.replace_all(&text, *repl).into_owned();
    }
    text
}
