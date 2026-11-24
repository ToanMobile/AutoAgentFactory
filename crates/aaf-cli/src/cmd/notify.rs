//! `aaf notify`: pushes the moments an unattended factory needs its owner to
//! the configured webhook and/or Telegram chats. It tails the event log with a
//! cursor kept in `<data_dir>/notify/`, so each moment is pushed once:
//!
//! - `factory_frozen`: the global freeze stopped every start;
//! - a task moved to `review`: work is waiting for a merge;
//! - a task moved to `blocked`: a human decision is needed.
//!
//! The factory already posts blocked escalations to the webhook itself, so
//! those go to Telegram only. The store is opened read-only; the first run
//! only records where the log ends, so history is never replayed.

use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{Clock, TaskStore};
use aaf_engine::db::Db;
use aaf_engine::telegram::send_alert;
use aaf_engine::webhook::{self, WebhookEvent, WebhookOptions};
use serde_json::{Value, json};

use super::board::store::{SharedRoDb, err, refuse_inside_agent_run};
use super::scope::resolve_scope;
use super::telegram::read_bot_token;
use crate::ctx::Ctx;
use crate::output::{CmdError, CmdOutput, CmdResult};

/// Events read per pass; a larger backlog is drained over the next passes.
const BATCH: u32 = 500;

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
}

/// One push: what happened, for which task, and the text the owner reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Alert {
    pub event_id: i64,
    pub kind: &'static str,
    pub event_kind: EventKind,
    pub task_id: Option<String>,
    pub text: String,
    /// False when the factory already posted the same moment to the webhook.
    pub to_webhook: bool,
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
    let (kind, text, to_webhook) = match event.kind {
        EventKind::FactoryFrozen => (
            "frozen",
            format!(
                "aaf: factory frozen after {} failed root task(s) in a row (last: {}); \
                 nothing starts until `aaf board unfreeze`",
                payload["consecutive_fails"].as_u64().unwrap_or(0),
                name(&task)
            ),
            true,
        ),
        EventKind::TaskStateChanged => match payload["to"].as_str() {
            Some("review") => (
                "review",
                format!("aaf: {} is ready for review", name(&task)),
                true,
            ),
            Some("blocked") => {
                let why = payload["why"].as_str().unwrap_or("blocked");
                let why: String = why.chars().take(300).collect();
                (
                    "blocked",
                    format!("aaf: {} needs you: {why}", name(&task)),
                    false,
                )
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
        to_webhook,
    })
}

fn cursor_path(ctx: &Ctx, scope_key: &str) -> PathBuf {
    ctx.data_dir
        .join("notify")
        .join(format!("{scope_key}.cursor"))
}

fn read_cursor(path: &Path) -> Result<Option<i64>, CmdError> {
    match fs::read_to_string(path) {
        Ok(text) => text
            .trim()
            .parse()
            .map(Some)
            .map_err(|e| err(&format!("cursor {}", path.display()), e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(err(&format!("cursor {}", path.display()), e)),
    }
}

fn write_cursor(path: &Path, id: i64) -> Result<(), CmdError> {
    if let Some(dir) = path.parent() {
        aaf_engine::db::private::create_dir(dir).map_err(|e| err("notify dir", e))?;
    }
    let tmp = path.with_extension("cursor.tmp");
    fs::write(&tmp, id.to_string()).map_err(|e| err("cursor", e))?;
    fs::rename(&tmp, path).map_err(|e| err("cursor", e))
}

/// Where alerts go; resolved once, before the first pass.
struct Channels {
    webhook_url: Option<String>,
    telegram: Option<(aaf_domain::telegram::TelegramConfig, String)>,
}

impl Channels {
    fn resolve(ctx: &Ctx, config_root: &Path) -> Result<Channels, CmdError> {
        let global = ctx.load_config(config_root)?.global;
        let url = global.notifications.webhook_url.trim().to_string();
        let tg = global.telegram;
        let telegram = if tg.enabled && !tg.allowed_chat_ids.is_empty() {
            let token = read_bot_token(&tg.bot_token_env, ctx)?;
            Some((tg, token))
        } else {
            None
        };
        Ok(Channels {
            webhook_url: (!url.is_empty()).then_some(url),
            telegram,
        })
    }

    fn is_empty(&self) -> bool {
        self.webhook_url.is_none() && self.telegram.is_none()
    }

    fn send(&self, alert: &Alert, now: i64) -> Result<(), String> {
        if let Some(url) = self.webhook_url.as_deref().filter(|_| alert.to_webhook) {
            let event = WebhookEvent {
                event_id: format!("aaf-notify-{}", alert.event_id),
                kind: alert.event_kind,
                task_id: alert.task_id.clone(),
                status: alert.kind.to_string(),
                summary: alert.text.clone(),
                created_at: now,
            };
            webhook::send(url, &event, &WebhookOptions::default()).map_err(|e| e.to_string())?;
        }
        if let Some((cfg, token)) = &self.telegram {
            send_alert(cfg, token, &alert.text).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

struct Pass {
    alerts: Vec<Alert>,
    sent: usize,
    cursor: i64,
    errors: Vec<String>,
}

fn one_pass(
    db: &Db,
    scope: Option<&str>,
    cursor_file: &Path,
    channels: Option<&Channels>,
) -> Result<Pass, CmdError> {
    let Some(cursor) = read_cursor(cursor_file)? else {
        let end = db.max_event_id(scope).map_err(|e| err("events", e))?;
        write_cursor(cursor_file, end)?;
        return Ok(Pass {
            alerts: Vec::new(),
            sent: 0,
            cursor: end,
            errors: Vec::new(),
        });
    };
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
        },
        crate::ctx::SystemClock.now(),
    );
    for event in &events {
        if let Some(alert) = alert_for(event, &title) {
            if let Some(channels) = channels {
                if let Err(e) = channels.send(&alert, now) {
                    // Keep the cursor before it: the next pass retries.
                    pass.errors.push(format!("event {}: {e}", alert.event_id));
                    break;
                }
                pass.sent += 1;
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
    CmdOutput::ok("ok", text.join("\n"))
        .field("alerts", json!(alerts))
        .field("sent", json!(pass.sent))
        .field("cursor", json!(pass.cursor))
        .field("dry_run", json!(dry_run))
        .field("errors", json!(pass.errors))
}

pub fn run(args: &NotifyArgs, ctx: &Ctx) -> CmdResult {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf notify") {
        return Ok(refused);
    }
    let (_scope, repo_root, config_root) = resolve_scope(args.repo.as_deref(), args.all, ctx)?;
    let channels = if args.dry_run {
        None
    } else {
        let channels = Channels::resolve(ctx, &config_root)?;
        if channels.is_empty() {
            return Err(CmdError::new(
                "no notification channel: set [notifications] webhook_url or enable [telegram] \
                 in ~/.config/autofactory/config.toml (or use --dry-run)",
            ));
        }
        Some(channels)
    };
    let key = repo_root
        .as_deref()
        .map_or_else(|| "all".to_string(), aaf_fp::repo_id);
    let scope_root = repo_root
        .as_deref()
        .map(|p| p.to_string_lossy().into_owned());
    let cursor_file = cursor_path(ctx, &key);
    let ro_db = SharedRoDb::for_ctx(ctx);
    loop {
        let pass = match ro_db.get()? {
            Some(db) => one_pass(&db, scope_root.as_deref(), &cursor_file, channels.as_ref())?,
            None => Pass {
                alerts: Vec::new(),
                sent: 0,
                cursor: 0,
                errors: Vec::new(),
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
