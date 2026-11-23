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
use std::fs;
use std::path::{Path, PathBuf};

use aaf_domain::blocked::BlockedReason;
use aaf_domain::budget::{BudgetCheck, RootBudget};
use aaf_domain::config::RepoConfig;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::gate::{ChangeKind, HeadLockReport};
use aaf_domain::ports::{BudgetStore, Clock, EventSink, PortError, RunStore, TaskStore};
use aaf_domain::task::{Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::factory::prompt::PLAN_APPROVED_PREFIX;
use aaf_engine::factory::snapshots::steer_task;
use aaf_engine::factory::{is_frozen, unfreeze};
use aaf_engine::gate::{GIT_TIMEOUT, GateError, base, head_lock};
use aaf_engine::git::worktree::worktree_path;
use aaf_engine::handover::{load_handover, render_markdown};
use aaf_engine::telegram::{
    ApprovePreview, CommandHandler, HttpTelegramApi, LockedFileSummary, TelegramError,
    run_poll_loop, send_alert, validate_config,
};
use aaf_engine::telemetry::{CostRates, StatusQuery, collect_status_from_db};
use serde_json::{Value, json};

use super::board::store::{open_db, open_db_read_only, refuse_inside_agent_run};
use crate::ctx::{Ctx, SystemClock};
use crate::output::{CmdError, CmdOutput, CmdResult, notice};

const BOARD_MAX_ITEMS: usize = 30;
const SHOW_EVENTS_LIMIT: u32 = 5;
const PLAN_APPROVAL_PREFIX: &str = "plan approval required:";
const PLAN_EVENT_LOOKBACK: u32 = 100;

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
    let s = match run_poll_loop(&api, &poll_cfg, &mut handler, &SystemClock, max_polls) {
        Ok(s) => s,
        Err(err) => return map_telegram_err(err),
    };
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

fn read_bot_token(env_name: &str, ctx: &Ctx) -> Result<String, CmdError> {
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

fn resolve_scope(
    repo: Option<&Path>,
    all: bool,
    ctx: &Ctx,
) -> Result<(String, Option<PathBuf>, PathBuf), CmdError> {
    if all {
        return Ok(("all".to_string(), None, ctx.cwd.clone()));
    }
    let target = match repo {
        Some(r) => {
            let cand = if r.is_absolute() {
                r.to_path_buf()
            } else {
                ctx.cwd.join(r)
            };
            if !cand.exists() {
                return Err(CmdError::new(format!(
                    "--repo {} does not exist",
                    cand.display()
                )));
            }
            aaf_fp::repo_root(&cand).unwrap_or(cand)
        }
        None => match aaf_fp::repo_root(&ctx.cwd) {
            Ok(root) => root,
            Err(_) => return Ok(("all (outside git repo)".to_string(), None, ctx.cwd.clone())),
        },
    };
    let canon = fs::canonicalize(&target).unwrap_or(target);
    Ok((canon.display().to_string(), Some(canon.clone()), canon))
}

struct CliTelegramHandler<'a> {
    ctx: &'a Ctx,
    scope: String,
    repo_root: Option<String>,
}

impl CliTelegramHandler<'_> {
    fn db_ro(&self) -> Result<Option<Db>, String> {
        open_db_read_only(self.ctx).map_err(|e| e.message)
    }

    fn require_db_ro(&self) -> Result<Db, String> {
        self.db_ro()?
            .ok_or_else(|| "database not found".to_string())
    }

    fn db_rw(&self) -> Result<Db, String> {
        open_db(self.ctx).map_err(|e| e.message)
    }

    fn eval_head_lock(
        &self,
        task_id: &str,
    ) -> Result<(PathBuf, RepoConfig, HeadLockReport), String> {
        let db = self.require_db_ro()?;
        let task = db.get_task(task_id).map_err(s_err)?;
        let target_tree = resolve_task_tree(&db, &task, self.ctx);
        drop(db);

        let eff = self.ctx.load_config(&target_tree).map_err(|e| e.message)?;
        let base_info =
            base::resolve(&target_tree, &eff.repo.gate.base_ref, GIT_TIMEOUT).map_err(s_err)?;
        let report = head_lock::evaluate(
            &self.ctx.data_dir,
            &target_tree,
            &eff.repo,
            &base_info.diff_target,
        );
        Ok((target_tree, eff.repo, report))
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
        Ok(render_markdown(&data))
    }

    fn preview_approve(&self, task_id: &str) -> Result<ApprovePreview, String> {
        let (_, _, report) = self.eval_head_lock(task_id)?;
        let locked_changes = report
            .locked_changes
            .into_iter()
            .map(|ch| LockedFileSummary {
                change: change_code(ch.change).to_string(),
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
        let (target_tree, repo_cfg, current) = self.eval_head_lock(task_id)?;
        if current.locked_changes.is_empty() {
            return Err("no locked file is changed; nothing to approve".to_string());
        }
        if current.lock_fp != expected_lock_fp {
            return Err("locked diff changed since preview; run /approve again".to_string());
        }
        let reviewer = format!("telegram:{user_id}");
        match head_lock::approve(
            &self.ctx.data_dir,
            &target_tree,
            &repo_cfg,
            &reviewer,
            false,
            &SystemClock,
        ) {
            Ok(a) => Ok(format!(
                "approved {task_id} by {} (lock_fp={})",
                a.reviewer, a.lock_fp
            )),
            Err(GateError::NothingLocked) => {
                Err("no locked file is changed; nothing approved".to_string())
            }
            Err(e) => Err(e.to_string()),
        }
    }

    fn unblock(&mut self, task_id: &str, user_id: i64) -> Result<String, String> {
        let db = self.db_rw()?;
        let task = require_state(&db, task_id, TaskState::Blocked)?;
        let now = SystemClock.now();
        let budget_reset = reset_exhausted_budget(&db, &task.root_id).map_err(s_err)?;
        let plan_approved = maybe_approve_on_unblock(&db, &task, now).map_err(s_err)?;
        db.set_state(task_id, TaskState::Ready, None, now)
            .map_err(s_err)?;
        let p = json!({
            "to": "ready", "why": "aaf telegram unblock", "by": format!("telegram:{user_id}"),
            "was": task.blocked_reason, "budget_reset": budget_reset, "plan_approved": plan_approved,
        });
        emit_task_event(&db, task_id, EventKind::TaskStateChanged, &p, now)?;
        Ok(format!("{task_id} is ready"))
    }

    fn rework(&mut self, task_id: &str, reason: &str, user_id: i64) -> Result<String, String> {
        let (rerun, cleaned) = match reason.strip_prefix("--rerun") {
            Some(a) if a.is_empty() || a.starts_with(char::is_whitespace) => (true, a.trim()),
            _ => (false, reason.trim()),
        };
        if cleaned.is_empty() {
            return Err("rework reason is empty".to_string());
        }
        let db = self.db_rw()?;
        let task = require_state(&db, task_id, TaskState::Review)?;
        let target = if rerun {
            TaskState::Ready
        } else {
            TaskState::Gating
        };
        let now = SystemClock.now();
        let budget_reset = reset_exhausted_budget(&db, &task.root_id).map_err(s_err)?;
        clear_stale_receipts(&db, &task, self.ctx);
        let joined = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
        db.append_context_note(task_id, &format!("rework: {joined}"), now)
            .map_err(s_err)?;
        db.set_state(task_id, target, None, now).map_err(s_err)?;
        let by = format!("telegram:{user_id}");
        let p1 = json!({ "reason": cleaned, "to": target.as_str(), "rerun": rerun, "budget_reset": budget_reset, "by": by });
        emit_task_event(&db, task_id, EventKind::ReworkRequested, &p1, now)?;
        let p2 = json!({ "from": TaskState::Review.as_str(), "to": target.as_str(), "why": "aaf telegram rework", "by": by });
        emit_task_event(&db, task_id, EventKind::TaskStateChanged, &p2, now)?;
        Ok(format!("{task_id} sent back to {}", target.as_str()))
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

fn require_state(db: &Db, task_id: &str, expected: TaskState) -> Result<Task, String> {
    let task = db.get_task(task_id).map_err(s_err)?;
    if task.state != expected {
        return Err(format!(
            "refused: {task_id} is {}, not {}",
            task.state.as_str(),
            expected.as_str()
        ));
    }
    Ok(task)
}

fn s_err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn emit_task_event(
    db: &Db,
    task_id: &str,
    kind: EventKind,
    payload: &Value,
    now: i64,
) -> Result<(), String> {
    db.emit(&FactoryEvent::new(kind, payload.to_string(), now).for_task(task_id))
        .map_err(s_err)?;
    Ok(())
}

fn change_code(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "A",
        ChangeKind::Modified => "M",
        ChangeKind::Deleted => "D",
        ChangeKind::Renamed => "R",
    }
}

fn load_repo_config(ctx: &Ctx, task_repo: &Path) -> RepoConfig {
    let from_dir = |p: &Path| p.is_dir().then(|| ctx.load_config(p).ok()).flatten();
    if let Some(eff) = from_dir(task_repo) {
        return eff.repo;
    }
    ctx.repo_root()
        .ok()
        .and_then(|root| ctx.load_config(&root).ok())
        .map_or_else(RepoConfig::default, |eff| eff.repo)
}

fn resolve_task_tree(db: &Db, task: &Task, ctx: &Ctx) -> PathBuf {
    if let Ok(wt) = worktree_path(Path::new(&task.repo_root), &ctx.worktrees_dir, &task.id)
        && wt.is_dir()
    {
        return wt;
    }
    db.runs_for_task(&task.id)
        .ok()
        .and_then(|runs| {
            runs.into_iter()
                .rev()
                .map(|r| PathBuf::from(r.worktree_path))
                .find(|p| !p.as_os_str().is_empty() && p.is_dir())
        })
        .unwrap_or_else(|| PathBuf::from(&task.repo_root))
}

fn reset_exhausted_budget(db: &Db, root_id: &str) -> Result<bool, PortError> {
    match db.get_budget(root_id) {
        Ok(b) if b.check() != BudgetCheck::Ok => {
            db.put_budget(&RootBudget {
                tokens_used: 0,
                wall_secs_used: 0,
                cycles_used: 0,
                frozen: false,
                ..b
            })?;
            Ok(true)
        }
        Ok(_) | Err(PortError::NotFound(_)) => Ok(false),
        Err(other) => Err(other),
    }
}

fn clear_stale_receipts(db: &Db, task: &Task, ctx: &Ctx) {
    let mut roots = vec![PathBuf::from(&task.repo_root)];
    if let Ok(wt) = worktree_path(Path::new(&task.repo_root), &ctx.worktrees_dir, &task.id) {
        roots.push(wt);
    }
    if let Ok(runs) = db.runs_for_task(&task.id) {
        roots.extend(
            runs.into_iter()
                .filter(|r| !r.worktree_path.is_empty())
                .map(|r| PathBuf::from(r.worktree_path)),
        );
    }
    for root in roots {
        let _ = fs::remove_file(aaf_fp::receipt_path(&ctx.data_dir, &root));
        let _ = fs::remove_dir_all(ctx.data_dir.join("approvals").join(aaf_fp::repo_id(&root)));
    }
}

fn maybe_approve_on_unblock(db: &Db, task: &Task, now: i64) -> Result<bool, PortError> {
    let Some(BlockedReason::NeedsHuman { why }) = &task.blocked_reason else {
        return Ok(false);
    };
    let Some(rest) = why.strip_prefix(PLAN_APPROVAL_PREFIX) else {
        return Ok(false);
    };
    let generated = db
        .events_for_task(&task.id, PLAN_EVENT_LOOKBACK)?
        .into_iter()
        .filter(|e| e.kind == EventKind::PlanGenerated)
        .find_map(|e| {
            serde_json::from_str::<Value>(&e.payload_json)
                .ok()
                .and_then(|v| v["plan"].as_str().map(str::trim).map(str::to_string))
                .filter(|s| !s.is_empty())
        });
    let fallback = Some(rest.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or(why.as_str());
    let plan = generated.as_deref().unwrap_or(fallback);
    db.append_context_note(&task.id, &format!("{PLAN_APPROVED_PREFIX} {plan}"), now)?;
    Ok(true)
}
