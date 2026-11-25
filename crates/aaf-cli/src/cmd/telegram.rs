//! `aaf telegram [--once] [--all | --repo <path>] [--send-test]`:
//! outbound-only Telegram Bot API remote companion (zero listening ports).
//!
//! Security invariants:
//! - Human-only: refuses immediately (exit 2) when `AAF_RUN_ID` is set.
//! - Config comes strictly from Tier 1 (`~/.config/autofactory/config.toml`).
//! - Bot token is read strictly from `env::var(&cfg.telegram.bot_token_env)`.
//! - Fail-closed when disabled or when `allowed_chat_ids` is empty.
//! - Approvals re-verify `lock_fp` on disk and sign as `"telegram:<user_id>"`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use aaf_domain::ports::{Clock, EventSink, RunStore, TaskStore};
use aaf_domain::task::TaskState;
use aaf_engine::db::Db;
use aaf_engine::factory::snapshots::steer_task;
use aaf_engine::factory::{is_frozen, unfreeze};
use aaf_engine::gate::GateError;
use aaf_engine::handover::{load_handover, render_markdown};
use aaf_engine::telegram::{
    ApprovePreview, CommandHandler, HttpTelegramApi, LockedFileSummary, TelegramError, ack_offset,
    run_poll_loop_with_retry, send_alert, validate_config,
};
use aaf_engine::telemetry::{CostRates, StatusQuery, collect_status_from_db};
use serde_json::json;

use super::board::store::{SharedRoDb, open_db, refuse_inside_agent_run};
use super::handover::load_repo_config;
use super::scope::resolve_scope;
use super::task_ops::{
    ApproveOpError, approve_and_regate, eval_task_head_lock, parse_rework_reason, rework_task,
    unblock_task,
};
use crate::ctx::{Ctx, SystemClock};
use crate::output::{CmdError, CmdOutput, CmdResult, notice};

const BOARD_MAX_ITEMS: usize = 30;
const SHOW_EVENTS_LIMIT: u32 = 5;

#[derive(Debug, clap::Args)]
pub struct TelegramArgs {
    /// Poll once and exit instead of running a continuous long-poll loop.
    #[arg(long)]
    pub once: bool,
    /// Aggregate across all repositories instead of scoping to the current repo.
    #[arg(long, conflicts_with = "repo")]
    pub all: bool,
    /// Scope status and board commands to a specific repository path.
    #[arg(long)]
    pub repo: Option<PathBuf>,
    /// Send a one-shot test notification to all allowed_chat_ids and exit.
    #[arg(long)]
    pub send_test: bool,
}

pub fn run(args: &TelegramArgs, ctx: &Ctx) -> CmdResult {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf telegram") {
        return Ok(refused);
    }

    let (scope, repo_root, config_base) = resolve_scope(args.repo.as_deref(), args.all, ctx)?;
    let tg_cfg = ctx.load_config(&config_base)?.global.telegram;
    if !tg_cfg.enabled {
        return map_telegram_err(TelegramError::Disabled);
    }
    if tg_cfg.allowed_chat_ids.is_empty() {
        return map_telegram_err(TelegramError::EmptyAllowlist);
    }

    let token = read_bot_token(&tg_cfg.bot_token_env, ctx)?;
    if let Err(err) = validate_config(&tg_cfg, &token) {
        return map_telegram_err(err);
    }

    if args.send_test {
        let msg = format!("AAF Telegram test alert (scope: {scope})");
        if let Err(err) = send_alert(&tg_cfg, &token, &msg) {
            return map_telegram_err(err);
        }
        let chats = tg_cfg.allowed_chat_ids.len();
        return Ok(
            CmdOutput::ok("ok", format!("sent test alert to {chats} chat(s)"))
                .field("scope", json!(scope))
                .field("chats", json!(chats)),
        );
    }

    let api = match HttpTelegramApi::new(&tg_cfg.api_base_url, token) {
        Ok(api) => api,
        Err(err) => return map_telegram_err(err),
    };
    let mut handler = CliTelegramHandler {
        ro_db: SharedRoDb::for_ctx(ctx),
        ctx,
        scope: scope.clone(),
        repo_root: repo_root.map(|p| p.to_string_lossy().into_owned()),
    };
    let (poll_cfg, max_polls) = if args.once {
        let mut c = tg_cfg;
        c.poll_timeout_secs = 0;
        (c, Some(1))
    } else {
        notice(
            "telegram",
            &format!(
                "polling outbound (scope={scope}, chats={})",
                tg_cfg.allowed_chat_ids.len()
            ),
        );
        (tg_cfg, None)
    };
    let s = match run_poll_loop_with_retry(
        &api,
        &poll_cfg,
        &mut handler,
        &SystemClock,
        max_polls,
        !args.once,
        |err, delay| {
            notice(
                "telegram",
                &format!(
                    "transient poll error ({err}); retrying in {}s",
                    delay.as_secs()
                ),
            );
            std::thread::sleep(delay);
        },
    ) {
        Ok(s) => s,
        Err(err) => return map_telegram_err(err),
    };
    if args.once {
        ack_offset(&api, s.next_offset);
    }
    let text = format!(
        "polls={} updates={} handled={} replies={} ignored_unauthorized={}",
        s.polls, s.updates_seen, s.commands_handled, s.replies_sent, s.ignored_unauthorized
    );
    Ok(CmdOutput::ok("ok", text)
        .field("scope", json!(scope))
        .field("polls", json!(s.polls))
        .field("updates_seen", json!(s.updates_seen))
        .field("commands_handled", json!(s.commands_handled))
        .field("replies_sent", json!(s.replies_sent))
        .field("ignored_unauthorized", json!(s.ignored_unauthorized)))
}

pub(crate) fn read_bot_token(env_name: &str, ctx: &Ctx) -> Result<String, CmdError> {
    let key = env_name.trim();
    if key.is_empty() {
        return Err(CmdError::new("telegram.bot_token_env is empty"));
    }
    let non_empty = |v: String| (!v.trim().is_empty()).then_some(v);
    std::env::var(key)
        .ok()
        .and_then(non_empty)
        .or_else(|| {
            ctx.aaf_env
                .iter()
                .find(|(k, _)| k == key)
                .and_then(|(_, v)| non_empty(v.clone()))
        })
        .ok_or_else(|| CmdError::new(format!("environment variable {key} is not set or empty")))
}

fn map_telegram_err(err: TelegramError) -> CmdResult {
    match err {
        TelegramError::Disabled => Ok(CmdOutput::policy("disabled", err.to_string())),
        TelegramError::EmptyAllowlist => Ok(CmdOutput::policy("refused", err.to_string())),
        other => Err(CmdError::new(other.to_string())),
    }
}

struct CliTelegramHandler<'a> {
    ro_db: SharedRoDb,
    ctx: &'a Ctx,
    scope: String,
    repo_root: Option<String>,
}

impl CliTelegramHandler<'_> {
    fn db_ro(&self) -> Result<Option<Db>, String> {
        self.ro_db.get().map_err(|e| e.message)
    }

    fn require_db_ro(&self) -> Result<Db, String> {
        self.db_ro()?
            .ok_or_else(|| "database not found".to_string())
    }

    fn db_rw(&self) -> Result<Db, String> {
        open_db(self.ctx).map_err(|e| e.message)
    }
}

impl CommandHandler for CliTelegramHandler<'_> {
    fn status(&self, all: bool) -> Result<String, String> {
        let scope_label = if all { "all" } else { &self.scope };
        let Some(db) = self.db_ro()? else {
            return Ok(format!("scope: {scope_label}\nno database initialized yet"));
        };
        let repo = if all { None } else { self.repo_root.as_deref() };
        let q = StatusQuery {
            repo_root: repo,
            data_dir: Some(&self.ctx.data_dir),
            now: SystemClock.now(),
            rates: CostRates::default(),
        };
        let s = collect_status_from_db(&db, &q).map_err(s_err)?;
        let blocked = if s.blocked_reasons.is_empty() {
            "none".to_string()
        } else {
            s.blocked_reasons
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let (c, r) = (&s.counts, &s.runs);
        let lines = [
            format!("scope: {scope_label}"),
            format!(
                "tasks: {} total (ready={}, running={}, gating={}, review={}, blocked={}, done={})",
                c.total, c.ready, c.running, c.gating, c.review, c.blocked, c.done
            ),
            format!("blocked_reasons: {blocked}"),
            format!(
                "runs: {} total (running={}, ok={}, fail={})",
                r.total, r.running, r.succeeded, r.failed
            ),
            format!(
                "tokens: {} in + {} out (~${:.4})",
                s.tokens_in, s.tokens_out, s.estimated_cost_usd
            ),
            format!(
                "factory: frozen={}, accounts={}, slots={}",
                s.frozen,
                s.accounts.len(),
                s.throttle.throttle_slots
            ),
        ];
        Ok(lines.join("\n"))
    }

    fn board(&self, state: Option<&str>) -> Result<String, String> {
        let parsed = match state {
            None => None,
            Some(s) => Some(
                TaskState::parse(&s.to_ascii_lowercase())
                    .ok_or_else(|| format!("unknown state: {s}"))?,
            ),
        };
        let Some(db) = self.db_ro()? else {
            return Ok("no tasks (database not initialized)".to_string());
        };
        let tasks = db
            .list_tasks(self.repo_root.as_deref(), parsed)
            .map_err(s_err)?;
        let frozen = is_frozen(&db).map_err(s_err)?;
        if tasks.is_empty() {
            let msg = if frozen {
                "no tasks\nfactory is FROZEN"
            } else {
                "no tasks"
            };
            return Ok(msg.to_string());
        }
        let mut out = String::new();
        for t in tasks.iter().take(BOARD_MAX_ITEMS) {
            let _ = writeln!(
                out,
                "{} {} {} {}",
                t.id,
                t.priority.as_str(),
                t.state.as_str(),
                t.title
            );
        }
        if tasks.len() > BOARD_MAX_ITEMS {
            let _ = writeln!(out, "... (+{} more)", tasks.len() - BOARD_MAX_ITEMS);
        }
        if frozen {
            out.push_str("factory is FROZEN (/unfreeze)\n");
        }
        Ok(out.trim_end().to_string())
    }

    fn show(&self, task_id: &str) -> Result<String, String> {
        let db = self.require_db_ro()?;
        let t = db.get_task(task_id).map_err(s_err)?;
        let deps = db.deps_of(task_id).map_err(s_err)?;
        let runs = db.runs_for_task(task_id).map_err(s_err)?;
        let events = db
            .events_for_task(task_id, SHOW_EVENTS_LIMIT)
            .map_err(s_err)?;
        let mut out = format!(
            "{} [{} | {}]\ntitle: {}\nruns: {} | unblock_cycles: {}\n",
            t.id,
            t.state.as_str(),
            t.priority.as_str(),
            t.title,
            runs.len(),
            t.unblock_cycles
        );
        if let Some(reason) = &t.blocked_reason {
            let _ = writeln!(out, "blocked_reason: {}", reason.kind());
        }
        if !deps.is_empty() {
            let _ = writeln!(out, "deps: {}", deps.join(", "));
        }
        if !events.is_empty() {
            out.push_str("recent events:\n");
            for ev in &events {
                let _ = writeln!(out, "  - {:?} ({})", ev.kind, ev.created_at);
            }
        }
        Ok(out.trim_end().to_string())
    }

    fn handover(&self, task_id: &str) -> Result<String, String> {
        let db = self.require_db_ro()?;
        let task = db.get_task(task_id).map_err(s_err)?;
        let repo_cfg = load_repo_config(self.ctx, Path::new(&task.repo_root));
        let data = load_handover(&db, &self.ctx.data_dir, task_id, &repo_cfg).map_err(s_err)?;
        Ok(aaf_engine::handover::sanitize_external_markdown(
            &render_markdown(&data),
        ))
    }

    fn preview_approve(&self, task_id: &str) -> Result<ApprovePreview, String> {
        let (_, report) = eval_task_head_lock(self.require_db_ro()?, task_id, self.ctx)
            .map_err(|e| e.message().to_string())?;
        let locked_changes = report
            .locked_changes
            .into_iter()
            .map(|ch| LockedFileSummary {
                change: ch.change.letter().to_string(),
                path: ch.path,
            })
            .collect();
        Ok(ApprovePreview {
            task_id: task_id.to_string(),
            lock_fp: report.lock_fp,
            locked_changes,
        })
    }

    fn approve(
        &mut self,
        task_id: &str,
        expected_lock_fp: &str,
        user_id: i64,
    ) -> Result<String, String> {
        let db = self.db_rw()?;
        let reviewer = format!("telegram:{user_id}");
        match approve_and_regate(
            &db,
            self.ctx,
            task_id,
            &reviewer,
            Some(expected_lock_fp),
            "aaf telegram approve",
            &reviewer,
            SystemClock.now(),
        ) {
            Ok(out) => {
                let suffix = match out.transitioned_to {
                    Some(state) => format!(" -> {}", state.as_str()),
                    None => String::new(),
                };
                Ok(format!(
                    "approved {task_id} by {} (lock_fp={}){suffix}",
                    out.approval.reviewer, out.approval.lock_fp
                ))
            }
            Err(ApproveOpError::Gate(GateError::NothingLocked)) => {
                Err("no locked file is changed; nothing approved".to_string())
            }
            Err(ApproveOpError::Gate(GateError::LockFpChanged)) => {
                Err("locked diff changed since preview; run /approve again".to_string())
            }
            Err(e) => Err(e.to_string()),
        }
    }

    fn unblock(&mut self, task_id: &str, user_id: i64) -> Result<String, String> {
        let db = self.db_rw()?;
        let by = format!("telegram:{user_id}");
        unblock_task(&db, task_id, "aaf telegram unblock", &by, SystemClock.now())
            .map_err(|e| e.to_refused_string())?;
        Ok(format!("{task_id} is ready"))
    }

    fn unblock_all(&mut self, user_id: i64) -> Result<String, String> {
        let db = self.db_rw()?;
        let blocked = db
            .list_tasks(self.repo_root.as_deref(), Some(TaskState::Blocked))
            .map_err(s_err)?;
        if blocked.is_empty() {
            return Ok("no blocked tasks".to_string());
        }
        let by = format!("telegram:{user_id}");
        let now = SystemClock.now();
        let mut unblocked = Vec::with_capacity(blocked.len());
        for task in &blocked {
            unblock_task(&db, &task.id, "aaf telegram unblock all", &by, now)
                .map_err(|e| e.to_refused_string())?;
            unblocked.push(task.id.as_str());
        }
        Ok(format!(
            "unblocked {} blocked task(s): {}",
            unblocked.len(),
            unblocked.join(", ")
        ))
    }

    fn rework(&mut self, task_id: &str, reason: &str, user_id: i64) -> Result<String, String> {
        let (rerun, cleaned) = parse_rework_reason(reason);
        if cleaned.is_empty() {
            return Err("rework reason is empty".to_string());
        }
        let db = self.db_rw()?;
        let by = format!("telegram:{user_id}");
        let out = rework_task(
            &db,
            self.ctx,
            task_id,
            cleaned,
            rerun,
            "aaf telegram rework",
            &by,
            SystemClock.now(),
        )
        .map_err(|e| e.to_refused_string())?;
        Ok(format!("{task_id} sent back to {}", out.target.as_str()))
    }

    fn steer(&mut self, task_id: &str, note: &str, _user_id: i64) -> Result<String, String> {
        let db = self.db_rw()?;
        let recorded = steer_task(&db, task_id, note, SystemClock.now())?;
        Ok(format!("steered {task_id}: {recorded}"))
    }

    fn unfreeze(&mut self, _user_id: i64) -> Result<String, String> {
        let db = self.db_rw()?;
        unfreeze(&db, &SystemClock).map_err(s_err)?;
        Ok("factory is not frozen".to_string())
    }
}

fn s_err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
