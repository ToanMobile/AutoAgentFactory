//! `aaf ui`: zero-dependency authenticated local/LAN Web & Mobile Cockpit.
//! Human-only (refuses `AAF_RUN_ID`), enforces `--key`/`AAF_UI_TOKEN` on LAN
//! binds, re-verifies `lock_fp` on disk before signing, and honors `--read-only`.

mod helpers;

use std::fs;
use std::io::{self, ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use aaf_domain::config::RepoConfig;
use aaf_domain::event::EventKind;
use aaf_domain::gate::{ChangeKind, HeadLockReport};
use aaf_domain::ports::{Clock, EventSink, RunStore, TaskStore};
use aaf_domain::task::TaskState;
use aaf_engine::db::Db;
use aaf_engine::executors::stream_log::read_tail_lines;
use aaf_engine::factory::{is_frozen, snapshots::steer_task, unfreeze};
use aaf_engine::gate::{GIT_TIMEOUT, GateError, base, head_lock};
use aaf_engine::handover::{load_handover, render_markdown};
use aaf_engine::telemetry::{
    CostRates, RunCounts, StatusQuery, TaskCounts, collect_status_from_db, inspect_throttle,
};
use aaf_engine::watch::{WatchQuery, collect_watch_snapshot};
use aaf_engine::web_ui::{
    DEFAULT_HOST, DEFAULT_PORT, UiBackend, UiConfig, UiError, pairing_url, render_pairing_card,
    start_server,
};
use helpers::{
    clear_stale_receipts, collect_recent_runs, emit_task_event, format_blocked_detail,
    maybe_approve_on_unblock, require_state, reset_exhausted_budget, resolve_task_tree, s_err,
};
use serde_json::{Value, json};

use super::board::store::{open_db, open_db_read_only, refuse_inside_agent_run};
use crate::ctx::{Ctx, SystemClock};
use crate::output::{CmdError, CmdOutput, CmdResult};

const PLAN_APPROVAL_PREFIX: &str = "plan approval required:";
const PLAN_EVENT_LOOKBACK: u32 = 100;
const DETAIL_EVENTS_LIMIT: u32 = 25;
const RECENT_RUNS_LIMIT: usize = 20;
const POLL_TICK: Duration = Duration::from_millis(100);

#[derive(Debug, clap::Args)]
pub struct UiArgs {
    /// Bind address (default: 127.0.0.1; non-loopback requires --key or AAF_UI_TOKEN).
    #[arg(long, default_value = DEFAULT_HOST)]
    pub host: String,
    /// Listening TCP port (default: 4319).
    #[arg(long, default_value_t = DEFAULT_PORT)]
    pub port: u16,
    /// Explicit bearer token (required for non-loopback hosts; falls back to AAF_UI_TOKEN).
    #[arg(long)]
    pub key: Option<String>,
    /// Disable all mutating endpoints (approve, rework, steer, unblock, unfreeze).
    #[arg(long)]
    pub read_only: bool,
    /// Print a scannable terminal QR code alongside the pairing URL.
    #[arg(long)]
    pub qr: bool,
    /// Open the pairing URL in the default browser on macOS interactive TTYs.
    #[arg(long)]
    pub open: bool,
    /// Aggregate tasks and telemetry across all repositories.
    #[arg(long, conflicts_with = "repo")]
    pub all: bool,
    /// Scope cockpit data to a specific repository path.
    #[arg(long)]
    pub repo: Option<PathBuf>,
}

pub fn run(args: &UiArgs, ctx: &Ctx) -> CmdResult {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf ui") {
        return Ok(refused);
    }
    let (scope, repo_root, config_base) = resolve_scope(args.repo.as_deref(), args.all, ctx)?;
    let explicit_key = resolve_explicit_key(args.key.as_deref(), ctx);
    let cfg = match UiConfig::resolve(args.host.clone(), args.port, explicit_key, !args.read_only) {
        Ok(c) => c,
        Err(err @ UiError::InsecureLanBind { .. }) => {
            return Ok(CmdOutput::policy("refused", err.to_string()));
        }
        Err(err) => return Err(CmdError::new(err.to_string())),
    };
    let backend: Arc<dyn UiBackend> = Arc::new(CliUiBackend {
        ctx: ctx.clone(),
        scope: scope.clone(),
        repo_root,
        config_base,
        read_only: args.read_only,
    });
    let handle = start_server(cfg.clone(), backend).map_err(|e| CmdError::new(e.to_string()))?;
    let bound = handle.addr();
    let url = pairing_url(&cfg.host, bound.port(), &cfg.token);
    let card = render_pairing_card(&url, args.read_only, cfg.lan_mode, args.qr);
    eprintln!("{card}\nListening on http://{bound} (press q + Enter or Ctrl-C to stop)");
    #[cfg(target_os = "macos")]
    if args.open && ctx.stdin_is_tty {
        let _ = std::process::Command::new("open").arg(&url).spawn();
    }
    let quit = Arc::new(AtomicBool::new(false));
    if ctx.stdin_is_tty {
        let q = Arc::clone(&quit);
        let _ = thread::Builder::new()
            .name("aaf-ui-stdin".into())
            .spawn(move || {
                let (mut stdin, mut buf) = (io::stdin(), [0u8; 64]);
                while let Ok(n) = stdin.read(&mut buf) {
                    if n == 0 || buf[..n].iter().any(|b| matches!(b, b'q' | b'Q')) {
                        break;
                    }
                }
                q.store(true, Ordering::Relaxed);
            });
    }
    while !quit.load(Ordering::Relaxed) {
        thread::sleep(POLL_TICK);
    }
    handle.join();
    Ok(CmdOutput::ok("ok", "web cockpit stopped")
        .field("scope", json!(scope))
        .field("addr", json!(bound.to_string()))
        .field("read_only", json!(args.read_only)))
}

fn resolve_explicit_key(cli_key: Option<&str>, ctx: &Ctx) -> Option<String> {
    let clean = |v: &str| (!v.trim().is_empty()).then(|| v.trim().to_string());
    let env_val = ctx
        .aaf_env
        .iter()
        .find(|(k, _)| k == "AAF_UI_TOKEN")
        .map(|(_, v)| v.as_str());
    cli_key
        .and_then(clean)
        .or_else(|| env_val.and_then(clean))
        .or_else(|| std::env::var("AAF_UI_TOKEN").ok().and_then(|v| clean(&v)))
}

fn resolve_scope(
    repo: Option<&Path>,
    all: bool,
    ctx: &Ctx,
) -> Result<(String, Option<PathBuf>, PathBuf), CmdError> {
    if all {
        return Ok(("all".into(), None, ctx.cwd.clone()));
    }
    let target = match repo {
        Some(r) => {
            let cand = ctx.cwd.join(r);
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
            Err(_) => return Ok(("all (outside git repo)".into(), None, ctx.cwd.clone())),
        },
    };
    let canon = fs::canonicalize(&target).unwrap_or(target);
    Ok((canon.display().to_string(), Some(canon.clone()), canon))
}

struct CliUiBackend {
    ctx: Ctx,
    scope: String,
    repo_root: Option<PathBuf>,
    config_base: PathBuf,
    read_only: bool,
}

impl CliUiBackend {
    fn eval_head_lock(&self, id: &str) -> Result<(PathBuf, RepoConfig, HeadLockReport), String> {
        let db = self.require_db_ro()?;
        let task = db.get_task(id).map_err(|e| e.to_string())?;
        let tree = resolve_task_tree(&db, &task, &self.ctx);
        drop(db);
        let eff = self.ctx.load_config(&tree).map_err(|e| e.message)?;
        let base = base::resolve(&tree, &eff.repo.gate.base_ref, GIT_TIMEOUT)
            .map_err(|e| e.to_string())?;
        let rep = head_lock::evaluate(&self.ctx.data_dir, &tree, &eff.repo, &base.diff_target);
        Ok((tree, eff.repo, rep))
    }

    fn require_db_ro(&self) -> Result<Db, String> {
        open_db_read_only(&self.ctx)
            .map_err(|e| e.message)?
            .ok_or_else(|| "not found: database not initialized yet".into())
    }
}

impl UiBackend for CliUiBackend {
    fn meta_json(&self) -> Result<Value, String> {
        let repo = self.repo_root.as_deref().and_then(Path::to_str);
        let name = self
            .repo_root
            .as_deref()
            .and_then(Path::file_name)
            .and_then(|s| s.to_str())
            .unwrap_or(&self.scope);
        Ok(json!({
            "repo_name": name, "scope": self.scope, "repo_root": repo,
            "read_only": self.read_only, "version": env!("CARGO_PKG_VERSION"),
        }))
    }

    fn status_json(&self) -> Result<Value, String> {
        let db = open_db_read_only(&self.ctx).map_err(|e| e.message)?;
        let (now, repo) = (
            SystemClock.now(),
            self.repo_root.as_deref().and_then(Path::to_str),
        );
        let mut out = match db.as_ref() {
            Some(db) => {
                let q = StatusQuery {
                    repo_root: repo,
                    data_dir: Some(&self.ctx.data_dir),
                    now,
                    rates: CostRates::default(),
                };
                let s = collect_status_from_db(db, &q).map_err(|e| e.to_string())?;
                serde_json::to_value(s).map_err(|e| e.to_string())?
            }
            None => json!({
                "frozen": false, "consecutive_root_fails": 0, "repo_root": repo,
                "counts": TaskCounts::default(), "blocked_reasons": {},
                "runs": RunCounts::default(), "tokens_in": 0, "tokens_out": 0,
                "estimated_cost_usd": 0.0, "accounts": [],
                "throttle": inspect_throttle(&self.ctx.data_dir),
            }),
        };
        let eff = self.ctx.load_config(&self.config_base).ok();
        let wq = WatchQuery {
            scope: &self.scope,
            repo_root: self.repo_root.as_deref(),
            data_dir: &self.ctx.data_dir,
            selection: eff
                .as_ref()
                .map(|c| c.global.accounts.selection)
                .unwrap_or_default(),
            executor: eff
                .as_ref()
                .map_or("claude", |c| c.repo.run.executor.as_str()),
            now,
        };
        let watch = collect_watch_snapshot(&wq, db.as_ref()).ok();
        if let Value::Object(ref mut map) = out {
            let runs = db
                .as_ref()
                .map(|d| collect_recent_runs(d, repo))
                .unwrap_or_default();
            map.insert("scope".into(), json!(self.scope));
            map.insert(
                "heavy_slots".into(),
                json!(watch.as_ref().map(|w| &w.heavy_slots)),
            );
            map.insert(
                "watch_accounts".into(),
                json!(watch.as_ref().map(|w| &w.accounts)),
            );
            map.insert(
                "receipt".into(),
                json!(watch.as_ref().and_then(|w| w.receipt.as_ref())),
            );
            map.insert("recent_runs".into(), json!(runs));
        }
        Ok(out)
    }

    fn board_json(&self, state: Option<&str>) -> Result<Value, String> {
        let parsed = state
            .map(|s| {
                TaskState::parse(&s.to_ascii_lowercase())
                    .ok_or_else(|| format!("invalid state: {s}"))
            })
            .transpose()?;
        let Some(db) = open_db_read_only(&self.ctx).map_err(|e| e.message)? else {
            return Ok(json!({ "scope": self.scope, "frozen": false, "tasks": [] }));
        };
        let repo = self.repo_root.as_deref().and_then(Path::to_str);
        let tasks = db.list_tasks(repo, parsed).map_err(|e| e.to_string())?;
        let frozen = is_frozen(&db).map_err(|e| e.to_string())?;
        let items: Vec<Value> = tasks
            .into_iter()
            .map(|t| {
                let deps = db.deps_of(&t.id).unwrap_or_default();
                let latest_run_id = db.runs_for_task(&t.id).ok().and_then(|rs| {
                    rs.into_iter().max_by_key(|r| (r.started_at, r.id.clone())).map(|r| r.id)
                });
                let repo_name = Path::new(&t.repo_root)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("");
                let blocked_detail = t.blocked_reason.as_ref().map(format_blocked_detail);
                json!({
                    "id": t.id, "title": t.title, "state": t.state.as_str(),
                    "priority": t.priority.as_str(),
                    "blocked_reason": t.blocked_reason.as_ref().map(|b| b.kind()),
                    "blocked_detail": blocked_detail,
                    "spec_description": t.spec.description,
                    "acceptance": t.spec.acceptance,
                    "fingerprint": t.fingerprint,
                    "repo_name": repo_name,
                    "unblock_cycles": t.unblock_cycles, "deps": deps, "repo_root": t.repo_root,
                    "created_at": t.created_at, "updated_at": t.updated_at, "latest_run_id": latest_run_id,
                })
            })
            .collect();
        Ok(json!({ "scope": self.scope, "frozen": frozen, "tasks": items }))
    }

    fn task_detail_json(&self, task_id: &str) -> Result<Value, String> {
        let db = self.require_db_ro()?;
        Ok(json!({
            "task": db.get_task(task_id).map_err(s_err)?,
            "deps": db.deps_of(task_id).map_err(s_err)?,
            "runs": db.runs_for_task(task_id).map_err(s_err)?,
            "events": db.events_for_task(task_id, DETAIL_EVENTS_LIMIT).map_err(s_err)?,
        }))
    }

    fn handover_json(&self, task_id: &str) -> Result<Value, String> {
        let db = self.require_db_ro()?;
        let task = db.get_task(task_id).map_err(s_err)?;
        let task_repo = Path::new(&task.repo_root);
        let repo_cfg = task_repo
            .is_dir()
            .then(|| self.ctx.load_config(task_repo).ok())
            .flatten()
            .or_else(|| {
                self.ctx
                    .repo_root()
                    .ok()
                    .and_then(|r| self.ctx.load_config(&r).ok())
            })
            .map_or_else(RepoConfig::default, |eff| eff.repo);
        let data = load_handover(&db, &self.ctx.data_dir, task_id, &repo_cfg).map_err(s_err)?;
        let mut val = serde_json::to_value(&data).map_err(s_err)?;
        if let Value::Object(ref mut map) = val {
            map.insert("markdown".into(), json!(render_markdown(&data)));
        }
        Ok(val)
    }

    fn approve_preview_json(&self, task_id: &str) -> Result<Value, String> {
        let (_, _, report) = self.eval_head_lock(task_id)?;
        let changes: Vec<Value> = report
            .locked_changes
            .iter()
            .map(|ch| {
                let code = match ch.change {
                    ChangeKind::Added => "A",
                    ChangeKind::Modified => "M",
                    ChangeKind::Deleted => "D",
                    ChangeKind::Renamed => "R",
                };
                json!({ "change": code, "path": ch.path })
            })
            .collect();
        Ok(json!({
            "task_id": task_id, "lock_fp": report.lock_fp,
            "can_approve": !changes.is_empty(), "locked_changes": changes,
        }))
    }

    fn approve_commit(&self, id: &str, expected_fp: &str, reviewer: &str) -> Result<Value, String> {
        let (tree, repo_cfg, current) = self.eval_head_lock(id)?;
        if current.locked_changes.is_empty() {
            return Err("refused: no locked file is changed; nothing to approve".to_string());
        }
        if current.lock_fp != expected_fp {
            return Err("lock_fp mismatch: locked diff changed since preview".to_string());
        }
        let by: String = reviewer
            .trim()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
            .take(64)
            .collect();
        let who = if by.is_empty() { "web-ui" } else { &by };
        let res = head_lock::approve(
            &self.ctx.data_dir,
            &tree,
            &repo_cfg,
            who,
            false,
            &SystemClock,
        );
        match res {
            Ok(a) => Ok(
                json!({ "approved": true, "task_id": id, "reviewer": a.reviewer, "lock_fp": a.lock_fp }),
            ),
            Err(GateError::NothingLocked) => {
                Err("refused: no locked file is changed; nothing approved".into())
            }
            Err(e) => Err(e.to_string()),
        }
    }

    fn mutate_task(&self, action: &str, task_id: &str, note: &str) -> Result<Value, String> {
        let db = open_db(&self.ctx).map_err(|e| e.message)?;
        match action {
            "unblock" => do_unblock(&db, task_id),
            "rework" => do_rework(&db, &self.ctx, task_id, note),
            "steer" => {
                let recorded = steer_task(&db, task_id, note, SystemClock.now())?;
                Ok(json!({ "action": "steer", "task_id": task_id, "note": recorded }))
            }
            "unfreeze" => {
                unfreeze(&db, &SystemClock).map_err(s_err)?;
                Ok(json!({ "action": "unfreeze", "frozen": false }))
            }
            other => Err(format!("invalid action: {other}")),
        }
    }

    fn stream_tail_lines(&self, run_id: &str, max_lines: usize) -> Result<Vec<String>, String> {
        match read_tail_lines(&self.ctx.data_dir, run_id, max_lines) {
            Ok(lines) => Ok(lines),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.to_string()),
        }
    }
}

fn do_unblock(db: &Db, task_id: &str) -> Result<Value, String> {
    let task = require_state(db, task_id, TaskState::Blocked)?;
    let now = SystemClock.now();
    let budget_reset = reset_exhausted_budget(db, &task.root_id).map_err(s_err)?;
    let plan_approved = maybe_approve_on_unblock(db, &task, now).map_err(s_err)?;
    db.set_state(task_id, TaskState::Ready, None, now)
        .map_err(s_err)?;
    let p = json!({
        "to": "ready", "why": "aaf ui unblock", "by": "web-ui",
        "was": task.blocked_reason, "budget_reset": budget_reset, "plan_approved": plan_approved,
    });
    emit_task_event(db, task_id, EventKind::TaskStateChanged, &p, now)?;
    Ok(json!({ "action": "unblock", "task_id": task_id, "state": "ready" }))
}

fn do_rework(db: &Db, ctx: &Ctx, task_id: &str, reason: &str) -> Result<Value, String> {
    let (rerun, cleaned) = match reason.strip_prefix("--rerun") {
        Some(a) if a.is_empty() || a.starts_with(char::is_whitespace) => (true, a.trim()),
        _ => (false, reason.trim()),
    };
    if cleaned.is_empty() {
        return Err("invalid rework reason: empty".into());
    }
    let task = require_state(db, task_id, TaskState::Review)?;
    let target = if rerun {
        TaskState::Ready
    } else {
        TaskState::Gating
    };
    let now = SystemClock.now();
    let budget_reset = reset_exhausted_budget(db, &task.root_id).map_err(s_err)?;
    clear_stale_receipts(db, &task, ctx);
    let note = format!(
        "rework: {}",
        cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
    );
    db.append_context_note(task_id, &note, now).map_err(s_err)?;
    db.set_state(task_id, target, None, now).map_err(s_err)?;
    emit_task_event(
        db,
        task_id,
        EventKind::ReworkRequested,
        &json!({ "reason": cleaned, "to": target.as_str(), "rerun": rerun, "budget_reset": budget_reset, "by": "web-ui" }),
        now,
    )?;
    emit_task_event(
        db,
        task_id,
        EventKind::TaskStateChanged,
        &json!({ "from": "review", "to": target.as_str(), "why": "aaf ui rework", "by": "web-ui" }),
        now,
    )?;
    Ok(json!({ "action": "rework", "task_id": task_id, "state": target.as_str() }))
}
