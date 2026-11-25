//! `aaf ui`: zero-dependency authenticated local/LAN Web & Mobile Cockpit.
//! Human-only (refuses `AAF_RUN_ID`), enforces `--key`/`AAF_UI_TOKEN` on LAN
//! binds, re-verifies `lock_fp` on disk before signing, and honors `--read-only`.

pub(crate) mod actions;

use std::io::{self, ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use aaf_domain::ports::{Clock, EventSink, RunStore, TaskStore};
use aaf_domain::task::TaskState;
use aaf_engine::db::Db;
use aaf_engine::executors::stream_log::read_tail_lines;
use aaf_engine::factory::{snapshots::steer_task, unfreeze};
use aaf_engine::gate::GateError;
use aaf_engine::handover::{load_handover, render_markdown};
use aaf_engine::telemetry::{
    CostRates, RunCounts, StatusQuery, TaskCounts, collect_status_from_db_with_throttle,
    scan_throttle,
};
use aaf_engine::throttle::{LoadProbe, Pressure, SystemProbe};
use aaf_engine::watch::{WatchQuery, collect_watch_overlay_with_scan};
use aaf_engine::web_ui::qr::display_host_for_pairing;
use aaf_engine::web_ui::{
    APPROVE_DISABLED_MSG, DEFAULT_HOST, DEFAULT_PORT, UiBackend, UiConfig, UiError, pairing_url,
    render_pairing_card, start_server,
};
use serde_json::{Value, json};

use self::actions::{
    collect_recent_runs, do_approve_plan, do_rework, do_unblock, format_blocked_detail, s_err,
};
use super::board::store::{SharedRoDb, open_db, refuse_inside_agent_run};
use super::handover::load_repo_config;
use super::scope::resolve_scope;
use super::task_ops::{ApproveOpError, approve_and_regate, eval_task_head_lock, unblock_task};
use crate::ctx::{Ctx, SystemClock};
use crate::output::{CmdError, CmdOutput, CmdResult};

const DETAIL_EVENTS_LIMIT: u32 = 25;
const POLL_TICK: Duration = Duration::from_millis(100);
const STATUS_CACHE_TTL: Duration = Duration::from_secs(1);
const PRESSURE_CACHE_TTL: Duration = Duration::from_secs(5);

#[derive(Debug, clap::Args)]
pub struct UiArgs {
    /// Bind address (default: 127.0.0.1; non-loopback requires AAF_UI_TOKEN or --key).
    #[arg(long, default_value = DEFAULT_HOST)]
    pub host: String,
    /// Listening TCP port (default: 4319).
    #[arg(long, default_value_t = DEFAULT_PORT)]
    pub port: u16,
    /// Explicit bearer token (prefer AAF_UI_TOKEN env var; --key exposes the token in argv/ps).
    #[arg(long)]
    pub key: Option<String>,
    /// Disable all mutating endpoints (approve, rework, steer, unblock, unfreeze).
    #[arg(long)]
    pub read_only: bool,
    /// Opt in to POST /api/approve head-lock signing from the Web UI (disabled by default; ignored with --read-only).
    #[arg(long)]
    pub allow_approve: bool,
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
    if args.key.as_deref().is_some_and(|k| !k.trim().is_empty()) {
        eprintln!(
            "warning: --key exposes the bearer token in process argv (ps); prefer AAF_UI_TOKEN env var"
        );
    }
    let (scope, repo_root, config_base) = resolve_scope(args.repo.as_deref(), args.all, ctx)?;
    let explicit_key = resolve_explicit_key(args.key.as_deref(), ctx);
    let allow_approve = args.allow_approve && !args.read_only;
    let cfg = match UiConfig::resolve(args.host.clone(), args.port, explicit_key, !args.read_only) {
        Ok(c) => c.with_allow_approve(allow_approve),
        Err(err @ UiError::InsecureLanBind { .. }) => {
            return Ok(CmdOutput::policy("refused", err.to_string()));
        }
        Err(err) => return Err(CmdError::new(err.to_string())),
    };
    let effective_allow_approve = cfg.effective_allow_approve();
    let backend: Arc<dyn UiBackend> = Arc::new(CliUiBackend {
        ro_db: SharedRoDb::for_ctx(ctx),
        ctx: ctx.clone(),
        scope: scope.clone(),
        repo_root,
        config_base,
        read_only: args.read_only,
        allow_approve: effective_allow_approve,
        status_cache: Mutex::new(StatusCache::default()),
        status_refresh: Mutex::new(()),
        pressure_cache: Mutex::new(None),
    });
    let handle = start_server(cfg.clone(), backend).map_err(|e| CmdError::new(e.to_string()))?;
    let bound = handle.addr();
    let pairing_host = display_host_for_pairing(&cfg.host);
    let url = pairing_url(&pairing_host, bound.port(), &cfg.token);
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
        .field("read_only", json!(args.read_only))
        .field("allow_approve", json!(effective_allow_approve)))
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

#[derive(Default)]
struct StatusCache {
    generation: u64,
    entry: Option<(Instant, Value)>,
}

struct CliUiBackend {
    ro_db: SharedRoDb,
    ctx: Ctx,
    scope: String,
    repo_root: Option<PathBuf>,
    config_base: PathBuf,
    read_only: bool,
    allow_approve: bool,
    status_cache: Mutex<StatusCache>,
    status_refresh: Mutex<()>,
    pressure_cache: Mutex<Option<(Instant, Option<Pressure>)>>,
}

impl CliUiBackend {
    fn invalidate_status_cache(&self) {
        let mut guard = self.status_cache.lock().unwrap_or_else(|e| e.into_inner());
        guard.generation = guard.generation.wrapping_add(1);
        guard.entry = None;
    }

    fn cached_pressure(&self) -> Option<Pressure> {
        let mut guard = self
            .pressure_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some((at, reading)) = guard.as_ref()
            && at.elapsed() < PRESSURE_CACHE_TTL
        {
            return *reading;
        }
        let reading = SystemProbe::detect().pressure();
        *guard = Some((Instant::now(), reading));
        reading
    }

    fn read_cached_status(&self) -> Option<Value> {
        self.cached_or_generation().ok()
    }

    fn cached_or_generation(&self) -> Result<Value, u64> {
        let guard = self.status_cache.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .entry
            .as_ref()
            .filter(|(at, _)| at.elapsed() < STATUS_CACHE_TTL)
            .map(|(_, cached)| cached.clone())
            .ok_or(guard.generation)
    }

    fn store_status_if_current(&self, start_gen: u64, out: &Value) {
        let mut guard = self.status_cache.lock().unwrap_or_else(|e| e.into_inner());
        if guard.generation == start_gen {
            guard.entry = Some((Instant::now(), out.clone()));
        }
    }

    fn require_db_ro(&self) -> Result<Db, String> {
        self.ro_db
            .get()
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
            "read_only": self.read_only, "allow_approve": self.allow_approve,
            "version": env!("CARGO_PKG_VERSION"),
        }))
    }

    fn status_json(&self) -> Result<Value, String> {
        if let Some(cached) = self.read_cached_status() {
            return Ok(cached);
        }
        let _refresh = self
            .status_refresh
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let start_gen = match self.cached_or_generation() {
            Ok(cached) => return Ok(cached),
            Err(g) => g,
        };
        let db = self.ro_db.get().map_err(|e| e.message)?;
        let (now, repo) = (
            SystemClock.now(),
            self.repo_root.as_deref().and_then(Path::to_str),
        );
        let (throttle_snap, slot_entries) = scan_throttle(&self.ctx.data_dir);
        let drain_active = throttle_snap.drain_lock_present;
        let (mut out, accounts_summary) = match db.as_ref() {
            Some(db) => {
                let q = StatusQuery {
                    repo_root: repo,
                    data_dir: Some(&self.ctx.data_dir),
                    now,
                    rates: CostRates::default(),
                };
                let s = collect_status_from_db_with_throttle(db, &q, throttle_snap)
                    .map_err(|e| e.to_string())?;
                let val = serde_json::to_value(&s).map_err(|e| e.to_string())?;
                (val, s.accounts)
            }
            None => (
                json!({
                    "frozen": false, "consecutive_root_fails": 0, "repo_root": repo,
                    "counts": TaskCounts::default(), "blocked_reasons": {},
                    "runs": RunCounts::default(), "tokens_in": 0, "tokens_out": 0,
                    "estimated_cost_usd": 0.0, "accounts": [],
                    "throttle": throttle_snap,
                }),
                Vec::new(),
            ),
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
        let pressure = self.cached_pressure();
        let watch = collect_watch_overlay_with_scan(
            &wq,
            db.as_ref(),
            &accounts_summary,
            pressure,
            drain_active,
            &slot_entries,
        )
        .ok();
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
        self.store_status_if_current(start_gen, &out);
        Ok(out)
    }

    fn board_json(&self, state: Option<&str>) -> Result<Value, String> {
        let parsed = state
            .map(|s| {
                TaskState::parse(&s.to_ascii_lowercase())
                    .ok_or_else(|| format!("invalid state: {s}"))
            })
            .transpose()?;
        let Some(db) = self.ro_db.get().map_err(|e| e.message)? else {
            return Ok(json!({ "scope": self.scope, "frozen": false, "tasks": [] }));
        };
        let repo = self.repo_root.as_deref().and_then(Path::to_str);
        let tasks = db.list_tasks(repo, parsed).map_err(|e| e.to_string())?;
        let mut enrichment = db.board_enrichment(repo).map_err(|e| e.to_string())?;
        let items: Vec<Value> = tasks
            .into_iter()
            .map(|t| {
                let deps = enrichment.deps_by_task.remove(&t.id).unwrap_or_default();
                let latest_run_id = enrichment.latest_run_by_task.remove(&t.id);
                let repo_name = Path::new(&t.repo_root).file_name().and_then(|s| s.to_str()).unwrap_or("");
                let blocked_detail = t.blocked_reason.as_ref().map(format_blocked_detail);
                json!({
                    "id": t.id, "title": t.title, "state": t.state.as_str(), "priority": t.priority.as_str(),
                    "blocked_reason": t.blocked_reason.as_ref().map(|b| b.kind()), "blocked_detail": blocked_detail,
                    "spec_description": t.spec.description, "acceptance": t.spec.acceptance,
                    "fingerprint": t.fingerprint, "repo_name": repo_name, "unblock_cycles": t.unblock_cycles,
                    "deps": deps, "repo_root": t.repo_root, "created_at": t.created_at,
                    "updated_at": t.updated_at, "latest_run_id": latest_run_id,
                })
            })
            .collect();
        Ok(json!({ "scope": self.scope, "frozen": enrichment.frozen, "tasks": items }))
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
        let repo_cfg = load_repo_config(&self.ctx, Path::new(&task.repo_root));
        let data = load_handover(&db, &self.ctx.data_dir, task_id, &repo_cfg).map_err(s_err)?;
        let mut val = serde_json::to_value(&data).map_err(s_err)?;
        if let Value::Object(ref mut map) = val {
            map.insert("markdown".into(), json!(render_markdown(&data)));
        }
        Ok(val)
    }

    fn approve_preview_json(&self, task_id: &str) -> Result<Value, String> {
        let (_, report) = eval_task_head_lock(self.require_db_ro()?, task_id, &self.ctx)
            .map_err(|e| e.message().to_string())?;
        let changes: Vec<Value> = report
            .locked_changes
            .iter()
            .map(|ch| json!({ "change": ch.change.letter(), "path": ch.path }))
            .collect();
        Ok(json!({
            "task_id": task_id, "lock_fp": report.lock_fp,
            "can_approve": !changes.is_empty(), "locked_changes": changes,
        }))
    }

    fn approve_commit(&self, id: &str, expected_fp: &str, reviewer: &str) -> Result<Value, String> {
        if self.read_only || !self.allow_approve {
            return Err(format!("refused: {APPROVE_DISABLED_MSG}"));
        }
        let db = open_db(&self.ctx).map_err(|e| e.message)?;
        let by: String = reviewer
            .trim()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
            .take(64)
            .collect();
        let who = if by.is_empty() { "web-ui" } else { &by };
        match approve_and_regate(
            &db,
            &self.ctx,
            id,
            who,
            Some(expected_fp),
            "aaf ui approve",
            who,
            SystemClock.now(),
        ) {
            Ok(out) => {
                self.invalidate_status_cache();
                Ok(json!({
                    "approved": true, "task_id": id, "reviewer": out.approval.reviewer,
                    "lock_fp": out.approval.lock_fp, "state": out.transitioned_to.map(TaskState::as_str),
                }))
            }
            Err(ApproveOpError::Gate(GateError::NothingLocked)) => {
                Err("refused: no locked file is changed; nothing approved".into())
            }
            Err(ApproveOpError::Gate(GateError::LockFpChanged)) => {
                Err("lock_fp mismatch: locked diff changed since preview".into())
            }
            Err(e) => Err(e.to_string()),
        }
    }

    fn mutate_task(&self, action: &str, task_id: &str, note: &str) -> Result<Value, String> {
        let db = open_db(&self.ctx).map_err(|e| e.message)?;
        let res = match action {
            "unblock" => do_unblock(&db, task_id),
            "unblock_all" | "unblock-all" => {
                let repo = self.repo_root.as_deref().and_then(Path::to_str);
                let blocked = db
                    .list_tasks(repo, Some(TaskState::Blocked))
                    .map_err(s_err)?;
                let now = SystemClock.now();
                let mut ids = Vec::with_capacity(blocked.len());
                for t in &blocked {
                    unblock_task(&db, &t.id, "aaf ui unblock-all", "web-ui", now)
                        .map_err(|e| e.to_refused_string())?;
                    ids.push(t.id.clone());
                }
                Ok(json!({ "action": "unblock_all", "count": ids.len(), "task_ids": ids }))
            }
            "approve_plan" | "approve-plan" => do_approve_plan(&db, task_id),
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
        };
        if res.is_ok() {
            self.invalidate_status_cache();
        }
        res
    }

    fn stream_tail_lines(&self, run_id: &str, max_lines: usize) -> Result<Vec<String>, String> {
        match read_tail_lines(&self.ctx.data_dir, run_id, max_lines) {
            Ok(lines) => Ok(lines),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.to_string()),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn test_backend(tmp: &Path) -> CliUiBackend {
        let ctx = Ctx {
            cwd: tmp.to_path_buf(),
            home: tmp.join("home"),
            data_dir: tmp.join("data"),
            config_dir: tmp.join("config"),
            worktrees_dir: tmp.join("worktrees"),
            path_var: std::ffi::OsString::new(),
            run_id: None,
            stdin_is_tty: false,
            aaf_env: Vec::new(),
        };
        CliUiBackend {
            ro_db: SharedRoDb::for_ctx(&ctx),
            ctx,
            scope: "all".into(),
            repo_root: None,
            config_base: tmp.to_path_buf(),
            read_only: false,
            allow_approve: false,
            status_cache: Mutex::new(StatusCache::default()),
            status_refresh: Mutex::new(()),
            pressure_cache: Mutex::new(None),
        }
    }

    #[test]
    fn status_cache_returns_cached_value_until_invalidated() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = test_backend(tmp.path());
        let first = backend.status_json().unwrap();
        assert_eq!(first["scope"], json!("all"));
        assert!(backend.status_cache.lock().unwrap().entry.is_some());
        assert!(backend.pressure_cache.lock().unwrap().is_some());
        *backend.pressure_cache.lock().unwrap() = Some((Instant::now(), Some(Pressure::Warn)));
        assert_eq!(backend.cached_pressure(), Some(Pressure::Warn));
        let sentinel = Some((Instant::now(), json!({ "sentinel": 1 })));
        backend.status_cache.lock().unwrap().entry = sentinel;
        assert_eq!(backend.status_json().unwrap()["sentinel"], json!(1));
        backend.invalidate_status_cache();
        assert!(backend.status_cache.lock().unwrap().entry.is_none());
        let refreshed = backend.status_json().unwrap();
        assert_eq!(refreshed["scope"], json!("all"));
        assert!(refreshed.get("sentinel").is_none());
        assert_eq!(refreshed["heavy_slots"]["memory_pressure"], json!("warn"));
    }

    #[test]
    fn invalidation_during_inflight_refresh_skips_stale_cache_write() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = test_backend(tmp.path());
        let start_gen = backend.cached_or_generation().unwrap_err();
        backend.invalidate_status_cache();
        backend.store_status_if_current(start_gen, &json!({ "stale": true }));
        assert!(backend.read_cached_status().is_none());
        let cur_gen = backend.cached_or_generation().unwrap_err();
        assert_ne!(cur_gen, start_gen);
        backend.store_status_if_current(cur_gen, &json!({ "fresh": true }));
        assert_eq!(backend.read_cached_status(), Some(json!({ "fresh": true })));
    }
}
