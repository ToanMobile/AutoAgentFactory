//! `aaf notify`: pushes the moments an unattended factory needs its owner to
//! the configured webhook and/or Telegram chats. It tails the event log with a
//! cursor kept in `<data_dir>/notify/`, so each moment is pushed once:
//!
//! - `factory_frozen`: the global freeze stopped every start;
//! - a task moved to `review`: work is waiting for a merge;
//! - a task moved to `blocked`: a human decision is needed.
//!
//! Delivery is tracked per target (`webhook`, `tg:<chat_id>`) in `<key>.pending`
//! so a failing channel never duplicates alerts on healthy channels or wedges
//! the queue permanently. For Blocked events, `aaf notify` waits a 120 s grace
//! window for the factory's own `WebhookSent` event before posting to the
//! webhook. The store is opened read-only; the first real run only records
//! where the log ends, so history is never replayed.

mod deliver;
mod state;

use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{Clock, TaskStore};
use aaf_engine::db::Db;
use serde_json::{Value, json};

use super::board::store::{SharedRoDb, err, refuse_inside_agent_run};
use super::scope::resolve_scope;
use crate::ctx::Ctx;
use crate::output::{CmdError, CmdOutput, CmdResult};
use deliver::{Channels, deliver_alert, deliver_message};
use state::{
    PendingState, acquire_scope_lock, cursor_path, pending_path, read_cursor, read_pending,
    remove_pending, write_cursor, write_pending,
};

/// Events read per pass; a larger backlog is drained over the next passes.
const BATCH: u32 = 500;
/// Maximum characters sent by `aaf notify --message`.
const MAX_MESSAGE_CHARS: usize = 1000;

#[derive(Debug, clap::Args)]
pub struct NotifyArgs {
    /// Watch a specific repository path instead of the current directory.
    #[arg(long)]
    pub repo: Option<PathBuf>,
    /// Watch every repository in the database.
    #[arg(long, conflicts_with = "repo")]
    pub all: bool,
    /// Seconds between passes (default 30).
    #[arg(long, default_value_t = 30)]
    pub interval: u64,
    /// One pass, then exit.
    #[arg(long)]
    pub once: bool,
    /// Print what would be pushed; send nothing and keep the cursor.
    #[arg(long)]
    pub dry_run: bool,
    /// Send a single ad-hoc message to configured channels without touching the DB or cursor.
    #[arg(long, conflicts_with_all = ["once", "interval"])]
    pub message: Option<String>,
}

/// One push: what happened, for which task, and the text the owner reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Alert {
    pub event_id: i64,
    pub kind: &'static str,
    pub event_kind: EventKind,
    pub task_id: Option<String>,
    pub text: String,
    pub created_at: i64,
}

/// The alert for `event`, or `None` when it is not a "needs you" moment.
pub(crate) fn alert_for(event: &FactoryEvent, title: &dyn Fn(&str) -> String) -> Option<Alert> {
    let payload: Value = serde_json::from_str(&event.payload_json).unwrap_or(Value::Null);
    let id = event.id.unwrap_or_default();
    let task = event.task_id.clone();
    let name = |t: &Option<String>| {
        t.as_deref()
            .map(|t| format!("{t} ({})", title(t)))
            .unwrap_or_default()
    };
    let (kind, text) = match event.kind {
        EventKind::FactoryFrozen => {
            if payload["reason"].as_str() == Some("token_spike") {
                (
                    "frozen",
                    format!(
                        "aaf: factory frozen: token spike {} tokens in the last hour \
                         (limit {}); check for a runaway agent, then `aaf board unfreeze`",
                        payload["tokens_last_hour"].as_u64().unwrap_or(0),
                        payload["limit_last_hour"].as_u64().unwrap_or(0),
                    ),
                )
            } else {
                (
                    "frozen",
                    format!(
                        "aaf: factory frozen after {} failed root task(s) in a row (last: {}); \
                         nothing starts until `aaf board unfreeze`",
                        payload["consecutive_fails"].as_u64().unwrap_or(0),
                        name(&task)
                    ),
                )
            }
        }
        EventKind::CostPaused => {
            let resume = payload["resume_at"]
                .as_i64()
                .map(fmt_utc)
                .unwrap_or_else(|| "unknown".to_string());
            (
                "paused",
                format!(
                    "aaf: daily token cap reached ({}/{} in 24h); \
                     new work resumes automatically around {resume}",
                    payload["tokens_24h"].as_u64().unwrap_or(0),
                    payload["cap"].as_u64().unwrap_or(0),
                ),
            )
        }
        EventKind::CostResumed => (
            "resumed",
            format!(
                "aaf: daily token cap window cleared ({}/{} in 24h); new work resumes",
                payload["tokens_24h"].as_u64().unwrap_or(0),
                payload["cap"].as_u64().unwrap_or(0),
            ),
        ),
        EventKind::FactoryHalted => {
            let why = payload["why"].as_str().unwrap_or("halted");
            let why: String = why.chars().take(300).collect();
            (
                "halted",
                format!("aaf: factory halted on {}: {why}", name(&task)),
            )
        }
        EventKind::TaskStateChanged => match payload["to"].as_str() {
            Some("review") => (
                "review",
                format!("aaf: {} is ready for review", name(&task)),
            ),
            Some("blocked") => {
                let why = payload["why"].as_str().unwrap_or("blocked");
                let why: String = why.chars().take(300).collect();
                ("blocked", format!("aaf: {} needs you: {why}", name(&task)))
            }
            _ => return None,
        },
        _ => return None,
    };
    Some(Alert {
        event_id: id,
        kind,
        event_kind: event.kind,
        task_id: task,
        text,
        created_at: event.created_at,
    })
}

/// Formats a Unix timestamp in seconds as `YYYY-MM-DD HH:MM UTC` using
/// Howard Hinnant's civil-from-days algorithm (zero dependencies).
fn fmt_utc(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs_of_day = unix.rem_euclid(86_400);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {hour:02}:{minute:02} UTC")
}

struct Pass {
    alerts: Vec<Alert>,
    sent: usize,
    cursor: i64,
    errors: Vec<String>,
    cursor_reset: bool,
}

fn one_pass(
    db: &Db,
    scope: Option<&str>,
    cursor_file: &Path,
    pending_file: &Path,
    channels: Option<&Channels>,
) -> Result<Pass, CmdError> {
    let max_id = db.max_event_id(scope).map_err(|e| err("events", e))?;
    let Some(cursor) = read_cursor(cursor_file)? else {
        if channels.is_some() {
            write_cursor(cursor_file, max_id)?;
        }
        return Ok(Pass {
            alerts: Vec::new(),
            sent: 0,
            cursor: max_id,
            errors: Vec::new(),
            cursor_reset: false,
        });
    };
    if cursor > max_id {
        eprintln!(
            "aaf notify: event log was reset (cursor {cursor} > last event {max_id}); \
             resuming from {max_id}"
        );
        if channels.is_some() {
            write_cursor(cursor_file, max_id)?;
            remove_pending(pending_file);
        }
        return Ok(Pass {
            alerts: Vec::new(),
            sent: 0,
            cursor: max_id,
            errors: Vec::new(),
            cursor_reset: true,
        });
    }

    let events = db
        .events_after(scope, cursor, BATCH)
        .map_err(|e| err("events", e))?;
    let title = |id: &str| db.get_task(id).map(|t| t.title).unwrap_or_default();
    let (mut pass, now) = (
        Pass {
            alerts: Vec::new(),
            sent: 0,
            cursor,
            errors: Vec::new(),
            cursor_reset: false,
        },
        crate::ctx::SystemClock.now(),
    );

    let mut stored_pending = if channels.is_some() {
        read_pending(pending_file)
    } else {
        None
    };

    for event in &events {
        if let Some(alert) = alert_for(event, &title) {
            if let Some(channels) = channels {
                let mut pending = match stored_pending.take() {
                    Some(p) if p.event_id == alert.event_id => p,
                    Some(_) => {
                        remove_pending(pending_file);
                        PendingState::new(alert.event_id)
                    }
                    None => PendingState::new(alert.event_id),
                };
                let outcome = deliver_alert(db, channels, &alert, &mut pending, now)?;
                pass.errors.extend(outcome.errors);
                if outcome.any_sent {
                    pass.sent += 1;
                }
                if !outcome.finished {
                    write_pending(pending_file, &pending)?;
                    break;
                }
                remove_pending(pending_file);
            }
            pass.alerts.push(alert);
        }
        pass.cursor = event.id.unwrap_or(pass.cursor);
    }

    if channels.is_some() && pass.cursor != cursor {
        write_cursor(cursor_file, pass.cursor)?;
    }
    Ok(pass)
}

fn output(pass: &Pass, dry_run: bool) -> CmdOutput {
    let alerts: Vec<Value> = pass
        .alerts
        .iter()
        .map(|a| json!({ "event_id": a.event_id, "kind": a.kind, "task_id": a.task_id, "text": a.text }))
        .collect();
    let mut text: Vec<String> = pass.alerts.iter().map(|a| a.text.clone()).collect();
    if text.is_empty() {
        text.push("nothing new needs you".to_string());
    }
    text.extend(pass.errors.iter().map(|e| format!("not sent: {e}")));
    let mut out = if pass.errors.is_empty() {
        CmdOutput::ok("ok", text.join("\n"))
    } else {
        CmdOutput::policy("partial_error", text.join("\n"))
    };
    out = out
        .field("alerts", json!(alerts))
        .field("sent", json!(pass.sent))
        .field("cursor", json!(pass.cursor))
        .field("dry_run", json!(dry_run))
        .field("errors", json!(pass.errors));
    if pass.cursor_reset {
        out = out.field("cursor_reset", json!(true));
    }
    out
}

fn run_message(raw: &str, dry_run: bool, ctx: &Ctx) -> CmdResult {
    let text: String = raw.chars().take(MAX_MESSAGE_CHARS).collect();
    if dry_run {
        return Ok(CmdOutput::ok("ok", text.clone())
            .field("message", json!(text))
            .field("sent", json!(0))
            .field("dry_run", json!(true))
            .field("errors", json!(Vec::<String>::new())));
    }
    let channels = Channels::resolve_global(ctx)?;
    if channels.is_empty() {
        return Err(CmdError::new(
            "no notification channel: set [notifications] webhook_url or enable [telegram] \
             in ~/.config/autofactory/config.toml (or use --dry-run)",
        ));
    }
    let now = crate::ctx::SystemClock.now();
    let outcome = deliver_message(&channels, &text, now);
    let sent = usize::from(outcome.any_sent);
    let mut lines = vec![text.clone()];
    lines.extend(outcome.errors.iter().map(|e| format!("not sent: {e}")));
    let out = if outcome.errors.is_empty() {
        CmdOutput::ok("ok", lines.join("\n"))
    } else {
        CmdOutput::policy("partial_error", lines.join("\n"))
    };
    Ok(out
        .field("message", json!(text))
        .field("sent", json!(sent))
        .field("dry_run", json!(false))
        .field("errors", json!(outcome.errors)))
}

pub fn run(args: &NotifyArgs, ctx: &Ctx) -> CmdResult {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf notify") {
        return Ok(refused);
    }
    if let Some(raw_message) = &args.message {
        return run_message(raw_message, args.dry_run, ctx);
    }
    let (_scope, repo_root, config_root) = resolve_scope(args.repo.as_deref(), args.all, ctx)?;
    let key = repo_root
        .as_deref()
        .map_or_else(|| "all".to_string(), aaf_fp::repo_id);
    let (channels, _lock) = if args.dry_run {
        (None, None)
    } else {
        let channels = Channels::resolve(ctx, &config_root)?;
        if channels.is_empty() {
            return Err(CmdError::new(
                "no notification channel: set [notifications] webhook_url or enable [telegram] \
                 in ~/.config/autofactory/config.toml (or use --dry-run)",
            ));
        }
        let lock = acquire_scope_lock(ctx, &key)?;
        (Some(channels), Some(lock))
    };
    let scope_root = repo_root
        .as_deref()
        .map(|p| p.to_string_lossy().into_owned());
    let cursor_file = cursor_path(ctx, &key);
    let pending_file = pending_path(ctx, &key);
    let ro_db = SharedRoDb::for_ctx(ctx);
    loop {
        let pass = match ro_db.get()? {
            Some(db) => one_pass(
                &db,
                scope_root.as_deref(),
                &cursor_file,
                &pending_file,
                channels.as_ref(),
            )?,
            None => Pass {
                alerts: Vec::new(),
                sent: 0,
                cursor: 0,
                errors: Vec::new(),
                cursor_reset: false,
            },
        };
        if args.once {
            return Ok(output(&pass, args.dry_run));
        }
        for err_line in &pass.errors {
            eprintln!("aaf notify: not sent: {err_line}");
        }
        thread::sleep(Duration::from_secs(args.interval.max(1)));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn event(kind: EventKind, payload: Value, task_id: Option<&str>) -> FactoryEvent {
        let mut ev = FactoryEvent::new(kind, payload.to_string(), 1_700_000_000);
        ev.id = Some(42);
        if let Some(id) = task_id {
            ev = ev.for_task(id);
        }
        ev
    }

    #[test]
    fn fmt_utc_formats_epoch_and_leap_years() {
        assert_eq!(fmt_utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(fmt_utc(951_827_400), "2000-02-29 12:30 UTC");
        assert_eq!(fmt_utc(1_709_251_140), "2024-02-29 23:59 UTC");
        assert_eq!(fmt_utc(1_709_251_200), "2024-03-01 00:00 UTC");
    }

    #[test]
    fn alert_for_factory_frozen_formats_token_spike_and_preserves_legacy_text() {
        let spike = event(
            EventKind::FactoryFrozen,
            json!({
                "reason": "token_spike",
                "tokens_last_hour": 5000,
                "limit_last_hour": 4000,
                "consecutive_fails": 0,
                "threshold": 0
            }),
            None,
        );
        let alert = alert_for(&spike, &|_| String::new()).unwrap();
        assert_eq!(alert.kind, "frozen");
        assert_eq!(
            alert.text,
            "aaf: factory frozen: token spike 5000 tokens in the last hour (limit 4000); \
             check for a runaway agent, then `aaf board unfreeze`"
        );

        let legacy = event(
            EventKind::FactoryFrozen,
            json!({ "root_id": "t-9", "consecutive_fails": 3, "threshold": 3 }),
            Some("t-9"),
        );
        let legacy_alert = alert_for(&legacy, &|id| format!("Title {id}")).unwrap();
        assert_eq!(legacy_alert.kind, "frozen");
        assert_eq!(
            legacy_alert.text,
            "aaf: factory frozen after 3 failed root task(s) in a row (last: t-9 (Title t-9)); \
             nothing starts until `aaf board unfreeze`"
        );
    }

    #[test]
    fn alert_for_cost_paused_and_cost_resumed() {
        let paused = event(
            EventKind::CostPaused,
            json!({
                "tokens_24h": 1200,
                "cap": 1000,
                "resume_at": 1_709_251_140
            }),
            None,
        );
        let paused_alert = alert_for(&paused, &|_| String::new()).unwrap();
        assert_eq!(paused_alert.kind, "paused");
        assert_eq!(
            paused_alert.text,
            "aaf: daily token cap reached (1200/1000 in 24h); \
             new work resumes automatically around 2024-02-29 23:59 UTC"
        );

        let resumed = event(
            EventKind::CostResumed,
            json!({
                "tokens_24h": 0,
                "cap": 1000
            }),
            None,
        );
        let resumed_alert = alert_for(&resumed, &|_| String::new()).unwrap();
        assert_eq!(resumed_alert.kind, "resumed");
        assert_eq!(
            resumed_alert.text,
            "aaf: daily token cap window cleared (0/1000 in 24h); new work resumes"
        );
    }
}
