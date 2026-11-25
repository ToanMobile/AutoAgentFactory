//! Per-target alert delivery, permanent/transient error classification, and
//! Blocked webhook grace window for `aaf notify`.

use std::collections::HashSet;
use std::path::Path;

use aaf_domain::config::GlobalConfig;
use aaf_domain::event::EventKind;
use aaf_engine::db::Db;
use aaf_engine::telegram::{
    MAX_MESSAGE_BYTES, TelegramError, send_alert_each, truncate_message, validate_config,
};
use aaf_engine::web_ui::parse_trusted_origin;
use aaf_engine::webhook::{self, WebhookError, WebhookEvent, WebhookOptions};

use super::super::board::store::err;
use super::super::telegram::read_bot_token;
use super::Alert;
use super::state::PendingState;
use crate::ctx::Ctx;
use crate::output::CmdError;

/// Maximum transient-failure passes per target before dropping that target.
pub(super) const MAX_ATTEMPTS: u32 = 5;
/// Seconds to wait for the factory itself to record `WebhookSent` for a
/// freshly-Blocked event before `aaf notify` posts it to the webhook.
pub(super) const BLOCKED_GRACE_SECS: i64 = 120;

/// Returns `true` when `err` is permanent for webhook delivery (configuration,
/// serialization, or HTTP 4xx other than 408/429).
pub(super) fn is_permanent_webhook_error(err: &WebhookError) -> bool {
    match err {
        WebhookError::InvalidUrl { .. }
        | WebhookError::InvalidEvent(_)
        | WebhookError::Serialize(_) => true,
        WebhookError::Status { status, .. } => {
            (400..500).contains(status) && !matches!(*status, 408 | 429)
        }
        WebhookError::Transport { .. } => false,
    }
}

/// Returns `true` when `err` is permanent for `aaf notify` Telegram delivery.
///
/// Separate from `aaf_engine::telegram::is_transient_poll_error` so daemon
/// long-polling semantics stay untouched.
pub(super) fn is_permanent_telegram_error(err: &TelegramError) -> bool {
    match err {
        TelegramError::Disabled
        | TelegramError::EmptyAllowlist
        | TelegramError::MissingToken { .. }
        | TelegramError::InvalidToken
        | TelegramError::InvalidBaseUrl { .. }
        | TelegramError::Serialize(_) => true,
        TelegramError::HttpStatus { status, .. } => {
            (400..500).contains(status) && !matches!(*status, 408 | 429)
        }
        TelegramError::ApiError { code: Some(c), .. } => {
            (400..500).contains(c) && !matches!(*c, 408 | 429)
        }
        TelegramError::Transport { .. }
        | TelegramError::ResponseDecode { .. }
        | TelegramError::ApiError { code: None, .. } => false,
    }
}

/// Where alerts go; resolved once before the first pass.
pub(super) struct Channels {
    pub(super) webhook_url: Option<String>,
    pub(super) telegram: Option<(aaf_domain::telegram::TelegramConfig, String)>,
    pub(super) cockpit_url: Option<String>,
}

/// Validates Tier-1 `notifications.cockpit_url` as a strict HTTPS origin.
pub(super) fn parse_cockpit_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    match parse_trusted_origin(trimmed) {
        Ok(origin) => Some(origin.canonical),
        Err(e) => {
            eprintln!("aaf notify: ignoring invalid notifications.cockpit_url: {e}");
            None
        }
    }
}

/// Formats a Telegram alert body with an optional Cockpit deep-link suffix.
///
/// Never includes `#token=` or any secret. Truncates `text` on a valid UTF-8
/// boundary before appending the suffix when needed so the link stays intact
/// within [`MAX_MESSAGE_BYTES`].
pub(super) fn format_telegram_alert(
    text: &str,
    cockpit_url: Option<&str>,
    task_id: Option<&str>,
) -> String {
    let Some(base) = cockpit_url
        .map(|u| u.trim().trim_end_matches('/'))
        .filter(|u| !u.is_empty())
    else {
        return text.to_string();
    };
    let valid_tid = task_id
        .map(str::trim)
        .filter(|tid| tid.len() <= 128 && aaf_engine::git::worktree::validate_task_id(tid).is_ok());
    let suffix = match valid_tid {
        Some(id) => format!("\nCockpit: {base}/?task={id}"),
        None => format!("\nCockpit: {base}/?view=needyou"),
    };
    if text.len() + suffix.len() <= MAX_MESSAGE_BYTES {
        let mut out = String::with_capacity(text.len() + suffix.len());
        out.push_str(text);
        out.push_str(&suffix);
        return out;
    }
    let body_budget = MAX_MESSAGE_BYTES.saturating_sub(suffix.len());
    let truncated = truncate_message(text, body_budget);
    let mut out = String::with_capacity(truncated.len() + suffix.len());
    out.push_str(&truncated);
    out.push_str(&suffix);
    out
}

impl Channels {
    fn from_global(ctx: &Ctx, global: GlobalConfig) -> Channels {
        let url = global.notifications.webhook_url.trim().to_string();
        let cockpit_url = parse_cockpit_url(&global.notifications.cockpit_url);
        let tg = global.telegram;
        let telegram = if tg.enabled && !tg.allowed_chat_ids.is_empty() {
            match read_bot_token(&tg.bot_token_env, ctx) {
                Ok(token) => match validate_config(&tg, &token) {
                    Ok(()) => Some((tg, token)),
                    Err(e) => {
                        eprintln!("aaf notify: telegram disabled for this run: {e}");
                        None
                    }
                },
                Err(e) => {
                    eprintln!("aaf notify: telegram disabled for this run: {}", e.message);
                    None
                }
            }
        } else {
            None
        };
        Channels {
            webhook_url: (!url.is_empty()).then_some(url),
            telegram,
            cockpit_url,
        }
    }

    pub(super) fn resolve(ctx: &Ctx, config_root: &Path) -> Result<Channels, CmdError> {
        let global = ctx.load_config(config_root)?.global;
        Ok(Self::from_global(ctx, global))
    }

    /// Resolves notification channels strictly from Tier 1 global config so
    /// `--message` never fails when a repo's `.autofactory/config.toml` has
    /// syntax errors.
    pub(super) fn resolve_global(ctx: &Ctx) -> Result<Channels, CmdError> {
        let global = ctx.load_config(&ctx.config_dir)?.global;
        Ok(Self::from_global(ctx, global))
    }

    pub(super) fn is_empty(&self) -> bool {
        self.webhook_url.is_none() && self.telegram.is_none()
    }

    fn unique_chat_ids(&self) -> Vec<i64> {
        let Some((cfg, _)) = &self.telegram else {
            return Vec::new();
        };
        let mut seen = HashSet::new();
        cfg.allowed_chat_ids
            .iter()
            .copied()
            .filter(|id| seen.insert(*id))
            .collect()
    }
}

/// Result of attempting all unresolved targets for one alert during a pass.
pub(super) struct DeliverOutcome {
    pub(super) finished: bool,
    pub(super) any_sent: bool,
    pub(super) errors: Vec<String>,
}

enum WebhookPlan<'a> {
    Skip,
    WaitGrace,
    Send(&'a str),
}

fn plan_webhook<'a>(
    db: &Db,
    channels: &'a Channels,
    alert: &Alert,
    pending: &PendingState,
    now: i64,
) -> Result<WebhookPlan<'a>, CmdError> {
    let Some(url) = channels.webhook_url.as_deref() else {
        return Ok(WebhookPlan::Skip);
    };
    if pending.is_resolved("webhook") {
        return Ok(WebhookPlan::Skip);
    }
    if alert.kind != "blocked" {
        return Ok(WebhookPlan::Send(url));
    }
    if let Some(task_id) = alert.task_id.as_deref()
        && db
            .webhook_sent_ok_after(task_id, alert.event_id)
            .map_err(|e| err("events", e))?
    {
        return Ok(WebhookPlan::Skip);
    }
    if now.saturating_sub(alert.created_at) < BLOCKED_GRACE_SECS {
        return Ok(WebhookPlan::WaitGrace);
    }
    Ok(WebhookPlan::Send(url))
}

/// Sends `alert` to every target not yet in `pending.done ∪ pending.dropped`.
///
/// Updates `pending` in place and reports whether all targets are resolved
/// (`finished == true`) so the caller knows whether to advance the cursor or
/// persist `pending` and stop this pass.
pub(super) fn deliver_alert(
    db: &Db,
    channels: &Channels,
    alert: &Alert,
    pending: &mut PendingState,
    now: i64,
) -> Result<DeliverOutcome, CmdError> {
    let mut any_sent = false;
    let mut still_pending = false;
    let mut errors = Vec::new();

    let webhook_waiting_grace = match plan_webhook(db, channels, alert, pending, now)? {
        WebhookPlan::Skip => false,
        WebhookPlan::WaitGrace => true,
        WebhookPlan::Send(url) => {
            let event = WebhookEvent {
                event_id: format!("aaf-notify-{}", alert.event_id),
                kind: alert.event_kind,
                task_id: alert.task_id.clone(),
                status: alert.kind.to_string(),
                summary: alert.text.clone(),
                created_at: now,
            };
            // Retry is tracked across passes in `.pending`, not via inner backoff.
            let opts = WebhookOptions {
                max_retries: 0,
                ..WebhookOptions::default()
            };
            match webhook::send(url, &event, &opts) {
                Ok(()) => {
                    pending.mark_done("webhook");
                    any_sent = true;
                }
                Err(e) => {
                    record_failure(
                        pending,
                        alert.event_id,
                        "webhook",
                        is_permanent_webhook_error(&e),
                        &e.to_string(),
                        &mut errors,
                        &mut still_pending,
                    );
                }
            }
            false
        }
    };

    // Telegram sends immediately even while webhook waits for the Blocked grace window.
    if let Some((cfg, token)) = &channels.telegram {
        let chat_ids: Vec<i64> = channels
            .unique_chat_ids()
            .into_iter()
            .filter(|id| !pending.is_resolved(&format!("tg:{id}")))
            .collect();
        if !chat_ids.is_empty() {
            let tg_text = format_telegram_alert(
                &alert.text,
                channels.cockpit_url.as_deref(),
                alert.task_id.as_deref(),
            );
            match send_alert_each(cfg, token, &tg_text, &chat_ids) {
                Ok(outcomes) => {
                    for (chat_id, res) in outcomes {
                        let target = format!("tg:{chat_id}");
                        match res {
                            Ok(()) => {
                                pending.mark_done(&target);
                                any_sent = true;
                            }
                            Err(e) => {
                                record_failure(
                                    pending,
                                    alert.event_id,
                                    &target,
                                    is_permanent_telegram_error(&e),
                                    &e.to_string(),
                                    &mut errors,
                                    &mut still_pending,
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    let permanent = is_permanent_telegram_error(&e);
                    let detail = e.to_string();
                    for chat_id in chat_ids {
                        let target = format!("tg:{chat_id}");
                        record_failure(
                            pending,
                            alert.event_id,
                            &target,
                            permanent,
                            &detail,
                            &mut errors,
                            &mut still_pending,
                        );
                    }
                }
            }
        }
    }

    pending.waiting_grace = webhook_waiting_grace;
    Ok(DeliverOutcome {
        finished: !still_pending && !webhook_waiting_grace,
        any_sent,
        errors,
    })
}

/// Sends a one-shot `--message` to every configured target without touching
/// the database, cursor, or `.pending` state.
pub(super) fn deliver_message(channels: &Channels, text: &str, now: i64) -> DeliverOutcome {
    let mut any_sent = false;
    let mut errors = Vec::new();
    if let Some(url) = channels.webhook_url.as_deref() {
        let hash = aaf_fp::sha256_hex(text.as_bytes());
        let event = WebhookEvent {
            event_id: format!("aaf-message-{now}-{}", &hash[..8]),
            kind: EventKind::FactoryHalted,
            task_id: None,
            status: "message".to_string(),
            summary: text.to_string(),
            created_at: now,
        };
        let opts = WebhookOptions {
            max_retries: 0,
            ..WebhookOptions::default()
        };
        match webhook::send(url, &event, &opts) {
            Ok(()) => any_sent = true,
            Err(e) => {
                let kind = if is_permanent_webhook_error(&e) {
                    "permanent"
                } else {
                    "transient"
                };
                errors.push(format!("webhook ({kind}): {e}"));
            }
        }
    }
    if let Some((cfg, token)) = &channels.telegram {
        let chat_ids = channels.unique_chat_ids();
        if !chat_ids.is_empty() {
            match send_alert_each(cfg, token, text, &chat_ids) {
                Ok(outcomes) => {
                    for (chat_id, res) in outcomes {
                        match res {
                            Ok(()) => any_sent = true,
                            Err(e) => {
                                let kind = if is_permanent_telegram_error(&e) {
                                    "permanent"
                                } else {
                                    "transient"
                                };
                                errors.push(format!("tg:{chat_id} ({kind}): {e}"));
                            }
                        }
                    }
                }
                Err(e) => {
                    let kind = if is_permanent_telegram_error(&e) {
                        "permanent"
                    } else {
                        "transient"
                    };
                    let detail = e.to_string();
                    for chat_id in chat_ids {
                        errors.push(format!("tg:{chat_id} ({kind}): {detail}"));
                    }
                }
            }
        }
    }
    DeliverOutcome {
        finished: errors.is_empty(),
        any_sent,
        errors,
    }
}

fn record_failure(
    pending: &mut PendingState,
    event_id: i64,
    target: &str,
    permanent: bool,
    detail: &str,
    errors: &mut Vec<String>,
    still_pending: &mut bool,
) {
    if permanent {
        pending.mark_dropped(target);
        eprintln!("aaf notify: dropped {target} for event {event_id}: {detail}");
        errors.push(format!("event {event_id} ({target}): dropped: {detail}"));
        return;
    }
    let count = pending.bump_attempt(target);
    if count >= MAX_ATTEMPTS {
        pending.mark_dropped(target);
        eprintln!("aaf notify: dropped {target} for event {event_id}: {detail}");
        errors.push(format!(
            "event {event_id} ({target}): dropped after {count} attempts: {detail}"
        ));
    } else {
        *still_pending = true;
        errors.push(format!("event {event_id} ({target}): {detail}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhook_error_classification_covers_all_variants() {
        assert!(is_permanent_webhook_error(&WebhookError::InvalidUrl {
            url: "bad".into(),
            reason: "invalid",
        }));
        assert!(is_permanent_webhook_error(&WebhookError::InvalidEvent(
            "empty"
        )));
        assert!(is_permanent_webhook_error(&WebhookError::Serialize(
            "err".into()
        )));
        for code in [400, 401, 403, 404, 410, 422] {
            assert!(
                is_permanent_webhook_error(&WebhookError::Status {
                    url: "u".into(),
                    status: code,
                    attempts: 1,
                }),
                "HTTP {code} should be permanent"
            );
        }
        for code in [302, 408, 429, 500, 502, 503, 504] {
            assert!(
                !is_permanent_webhook_error(&WebhookError::Status {
                    url: "u".into(),
                    status: code,
                    attempts: 1,
                }),
                "HTTP {code} should be transient"
            );
        }
        assert!(!is_permanent_webhook_error(&WebhookError::Transport {
            url: "u".into(),
            detail: "reset".into(),
            attempts: 1,
        }));
    }

    #[test]
    fn telegram_error_classification_covers_every_variant() {
        // Permanent config/serialize variants.
        assert!(is_permanent_telegram_error(&TelegramError::Disabled));
        assert!(is_permanent_telegram_error(&TelegramError::EmptyAllowlist));
        assert!(is_permanent_telegram_error(&TelegramError::MissingToken {
            env_var: "TOK".into(),
        }));
        assert!(is_permanent_telegram_error(&TelegramError::InvalidToken));
        assert!(is_permanent_telegram_error(
            &TelegramError::InvalidBaseUrl {
                url: "bad".into(),
                reason: "invalid",
            }
        ));
        assert!(is_permanent_telegram_error(&TelegramError::Serialize(
            "err".into()
        )));

        // HttpStatus: 4xx except 408/429 are permanent; 3xx/408/429/5xx are transient.
        for code in [400, 401, 403, 404, 422] {
            assert!(
                is_permanent_telegram_error(&TelegramError::HttpStatus {
                    url: "u".into(),
                    status: code,
                }),
                "HttpStatus {code} should be permanent"
            );
        }
        for code in [301, 302, 408, 429, 500, 502, 503] {
            assert!(
                !is_permanent_telegram_error(&TelegramError::HttpStatus {
                    url: "u".into(),
                    status: code,
                }),
                "HttpStatus {code} should be transient"
            );
        }

        // ApiError: Some(4xx) except 408/429 are permanent; None/3xx/408/429/5xx are transient.
        for code in [400, 401, 403, 404] {
            assert!(
                is_permanent_telegram_error(&TelegramError::ApiError {
                    method: "sendMessage",
                    code: Some(code),
                    description: "err".into(),
                }),
                "ApiError {code} should be permanent"
            );
        }
        for code in [None, Some(302), Some(408), Some(429), Some(500), Some(503)] {
            assert!(
                !is_permanent_telegram_error(&TelegramError::ApiError {
                    method: "sendMessage",
                    code,
                    description: "err".into(),
                }),
                "ApiError {code:?} should be transient"
            );
        }

        // Transport and ResponseDecode are transient for notify.
        assert!(!is_permanent_telegram_error(&TelegramError::Transport {
            url: "u".into(),
            detail: "timeout".into(),
        }));
        assert!(!is_permanent_telegram_error(
            &TelegramError::ResponseDecode {
                method: "sendMessage",
                detail: "bad json".into(),
            }
        ));
    }
}
