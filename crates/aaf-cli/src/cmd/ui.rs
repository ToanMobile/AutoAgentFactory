//! `aaf ui`: zero-dependency authenticated local/LAN Web & Mobile Cockpit.
//! Human-only (refuses `AAF_RUN_ID`), enforces `--key`/`AAF_UI_TOKEN` on LAN
//! binds, re-verifies `lock_fp` on disk before signing, and honors `--read-only`.

pub(crate) mod accounts_panel;
mod actions;
mod decision_advice;
mod diff_parse;
mod extras;
mod proof_stop;
mod report_cache;
mod service;
mod service_env;
mod studio;
mod studio_health;
mod studio_ops;
mod studio_repos;
mod studio_tokens;
mod supervisor;
mod supervisor_ops;
mod tailscale;

use std::io::{self, ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use aaf_domain::ports::{Clock, EventSink, RunStore, TaskStore};
use aaf_engine::db::Db;
use aaf_engine::executors::stream_log::read_tail_lines;
use aaf_engine::telemetry::{
    RunCounts, StatusQuery, TaskCounts, collect_status_from_db_with_throttle, scan_throttle,
};
use aaf_engine::watch::{WatchQuery, collect_watch_overlay_with_scan};
use aaf_engine::web_ui::qr::display_host_for_pairing;
use aaf_engine::web_ui::{
    APPROVE_DISABLED_MSG, DEFAULT_HOST, DEFAULT_PORT, UiBackend, UiConfig, UiError,
    is_loopback_host, pairing_url, render_pairing_card, start_server,
};
use serde_json::{Value, json};

use self::actions::{collect_recent_runs, do_approve_commit, resolve_explicit_key, s_err};
use super::board::store::{SharedRoDb, open_db, refuse_inside_agent_run};
use super::scope::resolve_scope;
use crate::ctx::{Ctx, SystemClock};
use crate::output::{CmdError, CmdOutput, CmdResult};

const DETAIL_EVENTS_LIMIT: u32 = 100;
const POLL_TICK: Duration = Duration::from_millis(100);

#[derive(Debug, clap::Args)]
pub struct UiArgs {
    /// Bind address (default: 127.0.0.1; non-loopback requires AAF_UI_TOKEN or --key).
    #[arg(long, default_value = DEFAULT_HOST)]
    pub host: String,
    /// Listening TCP port (default: 4317).
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
    /// Run as an always-on loopback service with a persistent token in data_dir/ui-service.token.
    #[arg(
        long,
        conflicts_with_all = ["install_service", "uninstall_service", "service_url", "allow_approve", "key", "qr", "open"]
    )]
    pub service: bool,
    /// Install/restart the macOS launchd agent (`com.autoagentfactory.ui`) for the Cockpit service.
    #[arg(
        long,
        conflicts_with_all = ["service", "uninstall_service", "service_url", "allow_approve", "key", "open"]
    )]
    pub install_service: bool,
    /// Unload and remove the macOS launchd agent (`com.autoagentfactory.ui`).
    #[arg(long, conflicts_with_all = ["service", "install_service", "service_url"])]
    pub uninstall_service: bool,
    /// Print the fixed bookmark URL of the Cockpit service and exit.
    #[arg(long, conflicts_with_all = ["service", "install_service", "uninstall_service"])]
    pub service_url: bool,
    /// Opt in to board actions (ready, cancel, unblock, rework, steer, unfreeze) in `--service` / `--install-service` mode (approve remains locked).
    #[arg(long, conflicts_with_all = ["read_only", "allow_approve"])]
    pub allow_actions: bool,
    /// Trusted HTTPS reverse-proxy origin (e.g. Tailscale Serve `https://mac.tailnet.ts.net`).
    #[arg(long)]
    pub public_url: Option<String>,
    /// Configure persistent `tailscale serve --bg` and save the Tailscale HTTPS URL in Tier-1 config.
    #[arg(long, conflicts_with = "public_url")]
    pub tailscale: bool,
}

pub fn run(args: &UiArgs, ctx: &Ctx) -> CmdResult {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf ui") {
        return Ok(refused);
    }
    if args.uninstall_service {
        return service::run_uninstall(ctx);
    }
    if args.service_url {
        return service::run_service_url(ctx, args.port, args.public_url.as_deref(), args.qr);
    }
    if args.install_service {
        return service::run_install(
            ctx,
            args.allow_actions,
            args.public_url.as_deref(),
            args.tailscale,
            args.qr,
        );
    }
    if args.service && !is_loopback_host(&args.host) {
        return Ok(CmdOutput::policy(
            "refused",
            format!(
                "aaf ui --service must bind to loopback (127.0.0.1 / localhost / ::1), got '{}'",
                args.host
            ),
        ));
    }
    if args.key.as_deref().is_some_and(|k| !k.trim().is_empty()) {
        eprintln!(
            "warning: --key exposes the bearer token in process argv (ps); prefer AAF_UI_TOKEN env var"
        );
    }
    let scope_all = args.all || (args.service && args.repo.is_none());
    let (scope, repo_root, config_base) = resolve_scope(args.repo.as_deref(), scope_all, ctx)?;
    let read_only = args.read_only || (args.service && !args.allow_actions);
    let allow_approve = args.allow_approve && !read_only && !args.service;
    let explicit_key = if args.service {
        Some(
            service::load_or_migrate_token(&ctx.data_dir, &ctx.config_dir)
                .map_err(|e| CmdError::new(format!("service token: {e}")))?,
        )
    } else {
        resolve_explicit_key(args.key.as_deref(), ctx)
    };
    let trusted_origin =
        tailscale::resolve_public_origin(args.public_url.as_deref(), ctx).map_err(CmdError::new)?;
    let cfg = match UiConfig::resolve(args.host.clone(), args.port, explicit_key, !read_only) {
        Ok(c) => c
            .with_allow_approve(allow_approve)
            .with_trusted_origin(trusted_origin.clone()),
        Err(err @ UiError::InsecureLanBind { .. }) => {
            return Ok(CmdOutput::policy("refused", err.to_string()));
        }
        Err(err) => return Err(CmdError::new(err.to_string())),
    };
    let effective_allow_approve = cfg.effective_allow_approve();
    let supervisor = supervisor::UiSupervisor::new(ctx.clone(), repo_root.clone(), !read_only);
    let sup_shutdown = Arc::clone(&supervisor);
    let backend: Arc<dyn UiBackend> = Arc::new(CliUiBackend {
        ro_db: SharedRoDb::for_ctx(ctx),
        ctx: ctx.clone(),
        scope: scope.clone(),
        repo_root,
        config_base,
        read_only,
        allow_approve: effective_allow_approve,
        supervisor,
        server_started_at: SystemClock.now(),
        caches: report_cache::UiCaches::default(),
    });
    let handle = match start_server(cfg.clone(), backend) {
        Ok(h) => h,
        Err(UiError::Io(e)) if e.kind() == ErrorKind::AddrInUse => {
            sup_shutdown.shutdown();
            let msg = service::port_in_use_message(args.port, args.service, &ctx.data_dir);
            return Err(CmdError::new(msg));
        }
        Err(e) => {
            sup_shutdown.shutdown();
            return Err(CmdError::new(e.to_string()));
        }
    };
    let bound = handle.addr();
    let pairing_host = display_host_for_pairing(&cfg.host);
    let url = pairing_url(&pairing_host, bound.port(), &cfg.token);
    if args.service {
        service::trim_log_if_large(&ctx.home);
        let mode = if read_only {
            "read-only"
        } else {
            "actions enabled; approve locked"
        };
        let tok_path = service::token_path(&ctx.data_dir);
        eprintln!(
            "AAF Cockpit service ({mode}) listening on http://{bound} (scope: {scope}; token file: {})",
            tok_path.display()
        );
    } else {
        let card = render_pairing_card(&url, read_only, cfg.lan_mode, args.qr);
        eprintln!(
            "{card}\nHealth (monitoring & diagnostics): {}",
            studio::studio_url(&url)
        );
        if let Some(orig) = &trusted_origin {
            eprintln!(
                "{}",
                aaf_engine::web_ui::render_remote_pairing_card(
                    orig, &cfg.token, read_only, args.qr
                )
            );
        }
        eprintln!("Listening on http://{bound} (press q + Enter or Ctrl-C to stop)");
    }
    #[cfg(target_os = "macos")]
    if args.open && ctx.stdin_is_tty && !args.service {
        let _ = std::process::Command::new("open").arg(&url).spawn();
    }
    if ctx.stdin_is_tty && !args.service {
        let quit = Arc::new(AtomicBool::new(false));
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
        while !quit.load(Ordering::Relaxed) {
            thread::sleep(POLL_TICK);
        }
    } else {
        loop {
            thread::park();
        }
    }
    handle.join();
    sup_shutdown.shutdown();
    Ok(CmdOutput::ok("ok", "web cockpit stopped")
        .field("scope", json!(scope))
        .field("addr", json!(bound.to_string()))
        .field("read_only", json!(read_only))
        .field("allow_approve", json!(effective_allow_approve)))
}

struct CliUiBackend {
    ro_db: SharedRoDb,
    ctx: Ctx,
    scope: String,
    repo_root: Option<PathBuf>,
    config_base: PathBuf,
    read_only: bool,
    allow_approve: bool,
    supervisor: Arc<supervisor::UiSupervisor>,
    server_started_at: i64,
    caches: report_cache::UiCaches,
}

impl CliUiBackend {
    fn invalidate_status_cache(&self) {
        self.caches.invalidate_all();
    }

    fn require_db_ro(&self) -> Result<Db, String> {
        self.ro_db
            .get()
            .map_err(|e| e.message)?
            .ok_or_else(|| "not found: database not initialized yet".into())
    }
}

impl UiBackend for CliUiBackend {
    fn report_json(&self, since_hours: Option<u64>) -> Result<Value, String> {
        self.report_json_scoped(since_hours, None)
    }

    fn report_json_scoped(
        &self,
        since_hours: Option<u64>,
        req_repo: Option<&str>,
    ) -> Result<Value, String> {
        let db = self.ro_db.get().map_err(|e| e.message)?;
        let scoped = self.repo_root.as_deref().and_then(Path::to_str);
        studio::cached_report_json(
            &self.ctx,
            &self.config_base,
            db.as_ref(),
            scoped,
            req_repo,
            since_hours,
            !self.supervisor.is_enabled(),
            self.server_started_at,
            &self.caches,
        )
    }

    fn doctor_json(&self) -> Result<Value, String> {
        self.doctor_json_scoped(None)
    }

    fn doctor_json_scoped(&self, req_repo: Option<&str>) -> Result<Value, String> {
        let db = self.ro_db.get().map_err(|e| e.message)?;
        let scoped = self.repo_root.as_deref().and_then(Path::to_str);
        studio::cached_doctor_json(&self.ctx, db.as_ref(), scoped, req_repo, &self.caches)
    }

    fn meta_json(&self) -> Result<Value, String> {
        let repo = self.repo_root.as_deref().and_then(Path::to_str);
        let name = self
            .repo_root
            .as_deref()
            .and_then(Path::file_name)
            .and_then(|s| s.to_str())
            .unwrap_or(&self.scope);
        let db = self.ro_db.get().ok().flatten();
        let repos = extras::known_repos(db.as_ref(), self.repo_root.as_deref());
        Ok(json!({
            "repo_name": name, "scope": self.scope, "repo_root": repo,
            "read_only": self.read_only, "allow_approve": self.allow_approve,
            "repos": repos, "version": env!("CARGO_PKG_VERSION"),
        }))
    }

    fn status_json(&self) -> Result<Value, String> {
        if let Ok(cached) = self.caches.cached_status_or_gen() {
            return Ok(cached);
        }
        let _refresh = self
            .caches
            .status_refresh
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let start_gen = match self.caches.cached_status_or_gen() {
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
                let cost = self.ctx.cost_policy(db);
                let q = StatusQuery {
                    repo_root: repo,
                    data_dir: Some(&self.ctx.data_dir),
                    now,
                    cost: &cost,
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
                    "estimated_cost_usd": null, "cost_basis": "none", "accounts": [],
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
        let pressure = self.caches.cached_pressure();
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
            let freeze_thresh = eff
                .as_ref()
                .map_or(3, |c| c.repo.unblocker.global_freeze_consecutive_fails);
            map.insert("scope".into(), json!(self.scope));
            map.insert("freeze_threshold".into(), json!(freeze_thresh));
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
            map.insert("supervisor".into(), self.supervisor.status_json());
        }
        self.caches.store_status_if_current(start_gen, &out);
        Ok(out)
    }

    fn board_json(&self, state: Option<&str>) -> Result<Value, String> {
        let db = self.ro_db.get().map_err(|e| e.message)?;
        let repo = self.repo_root.as_deref().and_then(Path::to_str);
        actions::build_board_json(db.as_ref(), &self.scope, repo, state)
    }

    fn task_detail_json(&self, task_id: &str) -> Result<Value, String> {
        let db = self.require_db_ro()?;
        let task = db.get_task(task_id).map_err(s_err)?;
        let plan = extras::plan_detail_json(&db, &task);
        let decision_advice = decision_advice::read_for_task(&self.ctx, &task, &plan);
        Ok(json!({
            "task": task,
            "plan": plan,
            "decision_advice": decision_advice,
            "deps": db.deps_of(task_id).map_err(s_err)?,
            "runs": db.runs_for_task(task_id).map_err(s_err)?,
            "events": db.events_for_task(task_id, DETAIL_EVENTS_LIMIT).map_err(s_err)?,
        }))
    }

    fn task_diff_json(&self, task_id: &str) -> Result<Value, String> {
        extras::read_task_diff(&self.require_db_ro()?, &self.ctx, task_id)
    }

    fn task_proof_json(&self, task_id: &str) -> Result<Value, String> {
        proof_stop::read_task_proof(&self.require_db_ro()?, &self.ctx, task_id)
    }

    fn task_proof_image(&self, task_id: &str, idx: usize) -> Result<Vec<u8>, String> {
        proof_stop::read_task_proof_image(&self.require_db_ro()?, &self.ctx, task_id, idx)
    }

    fn create_task_json(&self, body: &Value) -> Result<Value, String> {
        if self.read_only {
            return Err("refused: server is running in read-only mode".into());
        }
        let res = extras::do_create_task(&self.ctx, self.repo_root.as_deref(), body)?;
        self.invalidate_status_cache();
        Ok(res)
    }

    fn accounts_json(&self, refresh: bool) -> Result<Value, String> {
        let db = if refresh && !self.read_only {
            None
        } else {
            self.ro_db.get().map_err(|e| e.message)?
        };
        let scoped = self.repo_root.as_deref();
        let res = accounts_panel::read_accounts_panel(
            db.as_ref(),
            &self.ctx,
            &self.config_base,
            scoped,
            self.read_only,
            refresh,
        )?;
        if refresh {
            self.invalidate_status_cache();
        }
        Ok(res)
    }

    fn handover_json(&self, task_id: &str) -> Result<Value, String> {
        actions::build_handover_json(&self.require_db_ro()?, &self.ctx, task_id)
    }

    fn approve_preview_json(&self, task_id: &str) -> Result<Value, String> {
        actions::build_approve_preview_json(self.require_db_ro()?, &self.ctx, task_id)
    }

    fn approve_commit(&self, id: &str, expected_fp: &str, reviewer: &str) -> Result<Value, String> {
        if self.read_only || !self.allow_approve {
            return Err(format!("refused: {APPROVE_DISABLED_MSG}"));
        }
        let db = open_db(&self.ctx).map_err(|e| e.message)?;
        let res = do_approve_commit(&db, &self.ctx, id, expected_fp, reviewer)?;
        self.invalidate_status_cache();
        Ok(res)
    }

    fn mutate_task(&self, action: &str, task_id: &str, note: &str) -> Result<Value, String> {
        if self.read_only {
            return Err("refused: server is running in read-only mode".into());
        }
        if matches!(action, "supervisor_toggle" | "supervisor_repos") {
            let res = self.supervisor.handle_action(action, note);
            if res.is_ok() {
                self.invalidate_status_cache();
            }
            return res;
        }
        if action.starts_with("cc_") {
            let res = accounts_panel::dispatch_cc_action(
                &self.ctx,
                self.repo_root.as_deref(),
                action,
                task_id,
                note,
            );
            if res.is_ok() {
                self.invalidate_status_cache();
            }
            return res;
        }
        let db = open_db(&self.ctx).map_err(|e| e.message)?;
        let repo = self.repo_root.as_deref().and_then(Path::to_str);
        let res = actions::dispatch_db_action(
            &db,
            &self.ctx,
            &self.config_base,
            repo,
            action,
            task_id,
            note,
        );
        if res.is_ok() {
            self.invalidate_status_cache();
        }
        res
    }

    fn task_pipeline_json(&self, task_id: &str) -> Result<Value, String> {
        actions::read_task_pipeline(&self.require_db_ro()?, task_id)
    }

    fn sop_runs_json(&self) -> Result<Value, String> {
        actions::read_sop_runs(&self.ctx)
    }

    fn mutate_task_with_comments(
        &self,
        action: &str,
        task_id: &str,
        note: &str,
        comments: &[Value],
    ) -> Result<Value, String> {
        let combined = actions::parse_and_format_comments(action, note, comments)?;
        self.mutate_task(action, task_id, &combined)
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
mod tests;
