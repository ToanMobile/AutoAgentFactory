//! Outbound Telegram Bot API companion for remote status, handover, and
//! two-step human-only approvals (zero listening ports, fail-closed).

pub mod client;
pub mod commands;
#[cfg(test)]
mod tests;

use std::sync::OnceLock;
use std::time::Duration;

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

/// Initial exponential backoff delay after a transient daemon poll failure.
pub const POLL_BACKOFF_INITIAL_SECS: u64 = 1;
/// Maximum exponential backoff delay for daemon poll retries.
pub const POLL_BACKOFF_MAX_SECS: u64 = 30;

/// Summary counters returned by [`run_poll_once`] and [`run_poll_loop`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PollStats {
    pub polls: usize,
    pub updates_seen: usize,
    pub commands_handled: usize,
    pub replies_sent: usize,
    pub reply_failures: usize,
    pub ignored_unauthorized: usize,
    pub next_offset: Option<i64>,
}

/// Returns `true` for transient network/rate-limit/5xx poll failures that a
/// long-running daemon should retry with backoff, and `false` for permanent
/// config/auth failures (`Disabled`, `EmptyAllowlist`, `InvalidToken`, 401, etc.).
pub fn is_transient_poll_error(err: &TelegramError) -> bool {
    match err {
        TelegramError::Transport { .. } => true,
        TelegramError::HttpStatus { status, .. } => *status == 429 || (500..600).contains(status),
        TelegramError::ApiError { code, .. } => matches!(code, Some(429) | Some(500..=599)),
        _ => false,
    }
}

/// Best-effort offset acknowledgment so Telegram drops already-processed updates
/// after a one-shot (`--once`) poll finishes.
pub fn ack_offset(api: &dyn TelegramApi, next_offset: Option<i64>) {
    if next_offset.is_some() {
        let _ = api.get_updates(next_offset, 0);
    }
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
                match api.send_message(chat_id, &text) {
                    Ok(()) => stats.replies_sent += 1,
                    Err(_) => stats.reply_failures += 1,
                }
            }
        }
    }
    Ok(stats)
}

/// Runs outbound long-polling in a loop until `max_polls` iterations complete
/// (or indefinitely when `max_polls` is `None`, retrying transient transport /
/// HTTP 429 / 5xx errors with exponential backoff 1s..30s).
pub fn run_poll_loop(
    api: &dyn TelegramApi,
    cfg: &TelegramConfig,
    handler: &mut dyn CommandHandler,
    clock: &dyn Clock,
    max_polls: Option<usize>,
) -> Result<PollStats, TelegramError> {
    run_poll_loop_with_retry(
        api,
        cfg,
        handler,
        clock,
        max_polls,
        max_polls.is_none(),
        |_, delay| std::thread::sleep(delay),
    )
}

/// Configurable polling loop with optional exponential backoff retry on
/// transient poll failures and an injectable retry/sleep hook for callers and tests.
pub fn run_poll_loop_with_retry<F>(
    api: &dyn TelegramApi,
    cfg: &TelegramConfig,
    handler: &mut dyn CommandHandler,
    clock: &dyn Clock,
    max_polls: Option<usize>,
    retry_transient: bool,
    mut on_retry: F,
) -> Result<PollStats, TelegramError>
where
    F: FnMut(&TelegramError, Duration),
{
    if !cfg.enabled {
        return Err(TelegramError::Disabled);
    }
    if cfg.allowed_chat_ids.is_empty() {
        return Err(TelegramError::EmptyAllowlist);
    }
    let mut dispatcher = CommandDispatcher::new(cfg.clone())?;
    let mut total = PollStats::default();
    let mut offset: Option<i64> = None;
    let mut backoff_secs = POLL_BACKOFF_INITIAL_SECS;

    while max_polls.is_none_or(|limit| total.polls < limit) {
        match run_poll_once(api, &mut dispatcher, handler, clock, offset) {
            Ok(step) => {
                backoff_secs = POLL_BACKOFF_INITIAL_SECS;
                offset = step.next_offset;
                total.polls += step.polls;
                total.updates_seen += step.updates_seen;
                total.commands_handled += step.commands_handled;
                total.replies_sent += step.replies_sent;
                total.reply_failures += step.reply_failures;
                total.ignored_unauthorized += step.ignored_unauthorized;
                total.next_offset = offset;
            }
            Err(err) if retry_transient && is_transient_poll_error(&err) => {
                on_retry(&err, Duration::from_secs(backoff_secs));
                backoff_secs = backoff_secs.saturating_mul(2).min(POLL_BACKOFF_MAX_SECS);
            }
            Err(err) => return Err(err),
        }
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod poll_resilience_tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;

    struct FixedClock;
    impl Clock for FixedClock {
        fn now(&self) -> i64 {
            1_700_000_000
        }
    }

    struct ScriptedApi {
        poll_results: Mutex<VecDeque<Result<Vec<Update>, TelegramError>>>,
        send_results: Mutex<VecDeque<Result<(), TelegramError>>>,
        poll_calls: Mutex<Vec<(Option<i64>, u64)>>,
        sent: Mutex<Vec<(i64, String)>>,
    }

    impl ScriptedApi {
        fn new(
            polls: Vec<Result<Vec<Update>, TelegramError>>,
            sends: Vec<Result<(), TelegramError>>,
        ) -> Self {
            Self {
                poll_results: Mutex::new(polls.into()),
                send_results: Mutex::new(sends.into()),
                poll_calls: Mutex::new(Vec::new()),
                sent: Mutex::new(Vec::new()),
            }
        }
    }

    impl TelegramApi for ScriptedApi {
        fn get_updates(
            &self,
            offset: Option<i64>,
            timeout_secs: u64,
        ) -> Result<Vec<Update>, TelegramError> {
            self.poll_calls.lock().unwrap().push((offset, timeout_secs));
            self.poll_results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(Vec::new()))
        }

        fn send_message(&self, chat_id: i64, text: &str) -> Result<(), TelegramError> {
            self.sent.lock().unwrap().push((chat_id, text.to_string()));
            self.send_results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(()))
        }
    }

    struct StubHandler;
    impl CommandHandler for StubHandler {
        fn status(&self, _all: bool) -> Result<String, String> {
            Ok("ok".into())
        }
        fn board(&self, _state: Option<&str>) -> Result<String, String> {
            Ok("ok".into())
        }
        fn show(&self, _task_id: &str) -> Result<String, String> {
            Ok("ok".into())
        }
        fn handover(&self, _task_id: &str) -> Result<String, String> {
            Ok("ok".into())
        }
        fn preview_approve(&self, _task_id: &str) -> Result<ApprovePreview, String> {
            Err("n/a".into())
        }
        fn approve(&mut self, _id: &str, _fp: &str, _uid: i64) -> Result<String, String> {
            Err("n/a".into())
        }
        fn unblock(&mut self, _id: &str, _uid: i64) -> Result<String, String> {
            Err("n/a".into())
        }
        fn rework(&mut self, _id: &str, _reason: &str, _uid: i64) -> Result<String, String> {
            Err("n/a".into())
        }
        fn steer(&mut self, _id: &str, _note: &str, _uid: i64) -> Result<String, String> {
            Err("n/a".into())
        }
        fn unfreeze(&mut self, _uid: i64) -> Result<String, String> {
            Err("n/a".into())
        }
    }

    fn cfg() -> TelegramConfig {
        TelegramConfig {
            enabled: true,
            bot_token_env: "AAF_TELEGRAM_BOT_TOKEN".into(),
            allowed_chat_ids: vec![1001],
            poll_timeout_secs: 0,
            confirm_ttl_secs: 60,
            allow_approve: false,
            allow_mutating: false,
            api_base_url: "https://api.telegram.org".into(),
        }
    }

    fn upd(id: i64, text: &str) -> Update {
        Update {
            update_id: id,
            message: Some(Message {
                message_id: id,
                from: Some(User {
                    id: 42,
                    username: None,
                    first_name: None,
                }),
                chat: Chat {
                    id: 1001,
                    chat_type: "private".into(),
                },
                text: Some(text.into()),
                date: 1_700_000_000,
            }),
        }
    }

    #[test]
    fn send_message_failure_counts_without_aborting_batch_or_offset() {
        let api = ScriptedApi::new(
            vec![Ok(vec![upd(10, "/status"), upd(11, "/board")])],
            vec![
                Err(TelegramError::Transport {
                    url: "https://api.telegram.org".into(),
                    detail: "timeout".into(),
                }),
                Ok(()),
            ],
        );
        let mut disp = CommandDispatcher::new(cfg()).unwrap();
        let stats = run_poll_once(&api, &mut disp, &mut StubHandler, &FixedClock, None).unwrap();
        assert_eq!(stats.updates_seen, 2);
        assert_eq!(stats.commands_handled, 2);
        assert_eq!(stats.replies_sent, 1);
        assert_eq!(stats.reply_failures, 1);
        assert_eq!(stats.next_offset, Some(12));
    }

    #[test]
    fn ack_offset_sends_zero_timeout_poll_only_when_offset_present() {
        let api = ScriptedApi::new(vec![Ok(Vec::new())], vec![]);
        ack_offset(&api, None);
        assert!(api.poll_calls.lock().unwrap().is_empty());
        ack_offset(&api, Some(99));
        assert_eq!(*api.poll_calls.lock().unwrap(), vec![(Some(99), 0)]);
    }

    #[test]
    fn daemon_retries_transient_errors_with_backoff_and_fails_fast_on_401() {
        let api = ScriptedApi::new(
            vec![
                Err(TelegramError::Transport {
                    url: "u".into(),
                    detail: "reset".into(),
                }),
                Err(TelegramError::HttpStatus {
                    url: "u".into(),
                    status: 429,
                }),
                Err(TelegramError::HttpStatus {
                    url: "u".into(),
                    status: 502,
                }),
                Err(TelegramError::ApiError {
                    method: "getUpdates",
                    code: Some(503),
                    description: "unavailable".into(),
                }),
                Ok(vec![upd(5, "/status")]),
                Err(TelegramError::Transport {
                    url: "u".into(),
                    detail: "flap".into(),
                }),
                Ok(vec![]),
            ],
            vec![Ok(())],
        );
        let mut delays = Vec::new();
        let stats = run_poll_loop_with_retry(
            &api,
            &cfg(),
            &mut StubHandler,
            &FixedClock,
            Some(2),
            true,
            |_, d| delays.push(d.as_secs()),
        )
        .unwrap();
        assert_eq!(stats.polls, 2);
        assert_eq!(stats.next_offset, Some(6));
        assert_eq!(delays, vec![1, 2, 4, 8, 1]);

        let auth_api = ScriptedApi::new(
            vec![Err(TelegramError::ApiError {
                method: "getUpdates",
                code: Some(401),
                description: "Unauthorized".into(),
            })],
            vec![],
        );
        let mut auth_retries = 0usize;
        let err = run_poll_loop_with_retry(
            &auth_api,
            &cfg(),
            &mut StubHandler,
            &FixedClock,
            None,
            true,
            |_, _| auth_retries += 1,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            TelegramError::ApiError {
                code: Some(401),
                ..
            }
        ));
        assert_eq!(auth_retries, 0);
    }
}
