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
        EventKind::FactoryFrozen => (
            "frozen",
            format!(
                "aaf: factory frozen after {} failed root task(s) in a row (last: {}); \
                 nothing starts until `aaf board unfreeze`",
                payload["consecutive_fails"].as_u64().unwrap_or(0),
                name(&task)
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
        // Only `exhausted` needs the owner: rework/skipped are handled by
        // the factory itself.
        EventKind::PrFeedback if payload["action"].as_str() == Some("exhausted") => (
            "pr_exhausted",
            format!(
                "aaf: {} stopped fixing PR #{} after {} feedback round(s); \
                 it stays in Review for you",
                name(&task),
                payload["pr"].as_u64().unwrap_or(0),
                payload["round"].as_u64().unwrap_or(0)
            ),
        ),
        EventKind::MergeResult if payload["code"].as_str() == Some("merge_retry_exhausted") => (
            "merge_stuck",
            format!(
                "aaf: {} could not be merged automatically ({}); it stays in Review for you",
                name(&task),
                payload["url"].as_str().unwrap_or("pull request")
            ),
        ),
        EventKind::TaskStateChanged => match payload["to"].as_str() {
            Some("review") => (
                "review",
                format!("aaf: {} is ready for review", name(&task)),
            ),
            // An infra block with a scheduled comeback (RFC-033) needs no one.
            Some("blocked") if payload.get("auto_retry_at").is_some() => return None,
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
mod tests_alert;
