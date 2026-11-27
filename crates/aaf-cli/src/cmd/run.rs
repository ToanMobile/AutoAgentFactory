//! `aaf run [GOAL] [--auto|--once] [--land MODE|--auto-merge] [--detach] [--auto-split] [--max-parallel N|auto] [--unsandboxed]`
//! (docs/PLAN.md C.10): wires the real store, executor, git and gate into
//! the factory loop of `aaf_engine::factory`.

mod deps;
pub(crate) mod detach;
mod goal;
mod progress;
pub(crate) mod report;
mod stage_wiring;
mod stop;
#[cfg(test)]
mod tests;

use std::fs::{File, TryLockError};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use aaf_domain::account::ProviderKind;
use aaf_domain::config::LandMode;
use aaf_domain::ports::TaskStore;
use aaf_domain::task::TaskState;
use aaf_engine::config::{self, ConfigInputs, ConfigOverlay, EffectiveConfig, RunOverlay};
use aaf_engine::db::{Db, private};
use aaf_engine::executors;
use aaf_engine::factory::stage_router::writer_hook_executors;
use aaf_engine::factory::{
    self, BindingView, Deps, Hooks, Mode, RunSummary, any_stage_uses_claude,
    configured_audit_provider, resolve_auto_audit_executor, resolve_base, same_audit_provider,
};
use serde_json::json;

use super::board::store::{err, open_db, refuse_inside_agent_run, repo_root};
use crate::ctx::Ctx;
use crate::output::{CmdError, CmdOutput, CmdResult};
use deps::build_deps;
pub(crate) use deps::run_executor_is_claude;
use report::{attach_auto_handover_reports, report};

/// How often `--auto` looks for new Ready tasks and ended cooldowns.
const AUTO_POLL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParallelArg {
    Fixed(u32),
    Auto,
}

impl std::str::FromStr for ParallelArg {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        if trimmed.eq_ignore_ascii_case("auto") {
            return Ok(Self::Auto);
        }
        let n: u32 = trimmed.parse().map_err(|_| {
            format!("invalid --max-parallel {s:?}: expected an integer >= 1 or `auto`")
        })?;
        if n == 0 {
            return Err("--max-parallel must be >= 1 or `auto` (got 0)".to_string());
        }
        Ok(Self::Fixed(n))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum LandArg {
    Branch,
    Pr,
    PrMerge,
    Auto,
}

impl From<LandArg> for LandMode {
    fn from(arg: LandArg) -> Self {
        match arg {
            LandArg::Branch => LandMode::Branch,
            LandArg::Pr => LandMode::Pr,
            LandArg::PrMerge => LandMode::PrMerge,
            LandArg::Auto => LandMode::Auto,
        }
    }
}

#[derive(Debug, clap::Args)]
pub struct RunArgs {
    /// Optional inline goal: creates a Ready task deterministically before running.
    #[arg(value_name = "GOAL")]
    pub goal: Option<String>,
    /// Keep running: look for Ready tasks until interrupted or frozen.
    #[arg(long, conflicts_with = "once")]
    pub auto: bool,
    /// Legacy single-pass mode: every Ready task gets at most one agent turn.
    #[arg(long, conflicts_with = "auto")]
    pub once: bool,
    /// Land clear tasks through the merge queue instead of opening a PR.
    #[arg(long, conflicts_with = "land")]
    pub auto_merge: bool,
    /// Landing strategy for gate-clear tasks (`branch`, `pr`, `pr-merge`, `auto`).
    #[arg(long, value_enum, value_name = "MODE")]
    pub land: Option<LandArg>,
    /// Agent runs at the same time (default: `run.max_parallel`; or `auto`).
    #[arg(long, value_name = "N|auto")]
    pub max_parallel: Option<ParallelArg>,
    /// Proactively split Ready root tasks into 2..=4 disjoint subtasks before running.
    #[arg(long)]
    pub auto_split: bool,
    /// Run agents without the Seatbelt sandbox. Required where there is none.
    #[arg(long)]
    pub unsandboxed: bool,
    /// Run the factory loop in the background and return once ready.
    #[arg(long)]
    pub detach: bool,
    /// Ask a running `aaf run` for this repository to drain in-flight work and stop cleanly.
    #[arg(
        long,
        conflicts_with_all = ["goal", "auto", "once", "detach", "detached_child"]
    )]
    pub stop: bool,
    /// Internal flag set when spawned by `--detach`.
    #[arg(long, hide = true)]
    pub detached_child: bool,
    /// Acceptance criterion for an inline GOAL (repeatable).
    #[arg(long = "accept", requires = "goal")]
    pub accept: Vec<String>,
    /// Priority for an inline GOAL (P0..P3, default P2).
    #[arg(long, requires = "goal")]
    pub priority: Option<String>,
    /// Relative path prefix owned by an inline GOAL (repeatable).
    #[arg(long = "scope", requires = "goal")]
    pub scope: Vec<String>,
}

/// Whether agent runs are sandboxed. macOS uses Seatbelt; Linux uses
/// Bubblewrap (`bwrap`) or an active hardened container; anywhere else running
/// unconfined has to be asked for by name (PLAN A.17).
pub(crate) fn sandbox_choice(
    unsandboxed: bool,
    host_sandbox: Result<bool, String>,
) -> Result<bool, String> {
    if unsandboxed {
        return Ok(false);
    }
    match host_sandbox {
        Ok(true) => Ok(true),
        Ok(false) => Err(
            "no agent sandbox on this platform: pass --unsandboxed to run agents unconfined"
                .to_string(),
        ),
        Err(why) => Err(why),
    }
}

pub(crate) fn probe_run_sandbox() -> Result<bool, String> {
    if cfg!(target_os = "macos") {
        Ok(true)
    } else if cfg!(target_os = "linux") {
        aaf_engine::executors::linux_sandbox::probe_host_linux_isolation()
            .map(|iso| iso.is_isolated())
    } else {
        Ok(false)
    }
}

fn load_config(args: &RunArgs, ctx: &Ctx, repo: &Path) -> Result<EffectiveConfig, CmdError> {
    let env = |key: &str| {
        ctx.aaf_env
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
    };
    let fixed_parallel = match args.max_parallel {
        Some(ParallelArg::Fixed(n)) => Some(n),
        Some(ParallelArg::Auto) | None => None,
    };
    let cli = ConfigOverlay {
        run: RunOverlay {
            auto_merge: args.auto_merge.then_some(true),
            land: args.land.map(LandMode::from),
            max_parallel: fixed_parallel,
            ..RunOverlay::default()
        },
        ..ConfigOverlay::default()
    };
    // Same tiers as `Ctx::load_config`, plus this command's flags as the
    // command-line overlay (the only place `--auto-merge` can switch it on
    // above tier 2).
    config::load(&ConfigInputs {
        global_config: Some(ctx.config_dir.join("config.toml")),
        repo_root: repo.to_path_buf(),
        env: &env,
        cli,
    })
    .map_err(|e| err("config", e))
}

/// One `aaf run` per repository: a second one would start the same tasks.
/// The lock dies with the process, like every other aaf lock.
fn run_lock(ctx: &Ctx, repo: &Path) -> Result<Option<File>, CmdError> {
    let dir = ctx.data_dir.join("locks");
    private::create_dir(&dir).map_err(|e| err("locks dir", e))?;
    let path = detach::run_lock_path(&ctx.data_dir, repo);
    let file = private::open_file(&path).map_err(|e| err(&path.display().to_string(), e))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(err(&path.display().to_string(), e)),
    }
}

/// Why tasks are waiting, when it is the repository's own account they wait
/// for: without it "1 waiting for an account" next to free accounts reads
/// like a bug.
pub(crate) fn waiting_line(waiting: u32, binding: &BindingView) -> Option<String> {
    let account = binding.account.as_ref().filter(|_| waiting > 0)?;
    let until = binding.cooldown_until?;
    Some(format!(
        "waiting for account {} (selection={}): in cooldown until {until}",
        account.name,
        binding.selection.as_str()
    ))
}

pub(crate) fn drain_stop_reason(summary: &RunSummary) -> &'static str {
    if summary.frozen {
        "frozen"
    } else if summary.halted.is_some() {
        "halted"
    } else if !summary.blocked.is_empty() {
        "blocked"
    } else if summary.waiting_for_review_cap > 0 {
        "waiting_for_review_cap"
    } else if summary.waiting_for_account > 0 {
        "waiting_for_account"
    } else {
        "drained"
    }
}

/// True when this repository has a task an agent could be started for.
fn has_work_in_db(db: &Db, repo: &Path) -> Result<bool, CmdError> {
    let repo_str = repo.to_string_lossy();
    for state in [TaskState::Ready, TaskState::Running, TaskState::Gating] {
        let tasks = db
            .list_tasks(Some(&repo_str), Some(state))
            .map_err(|e| err("store", e))?;
        if !tasks.is_empty() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// True when this repository has a task an agent could be started for.
fn has_work(deps: &Deps) -> Result<bool, CmdError> {
    let repo = Some(deps.settings.repo_root.as_str());
    for state in [TaskState::Ready, TaskState::Running, TaskState::Gating] {
        let tasks = deps
            .store
            .list_tasks(repo, Some(state))
            .map_err(|e| err("store", e))?;
        if !tasks.is_empty() {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn run(args: &RunArgs, ctx: &Ctx) -> CmdResult {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf run") {
        let reason = refused.text.clone();
        return Ok(refused
            .field("refused_kind", json!("agent_run"))
            .field("reason", json!(reason)));
    }
    if args.stop {
        return stop::request_stop(ctx);
    }
    let sandboxed = match sandbox_choice(args.unsandboxed, probe_run_sandbox()) {
        Ok(sandboxed) => sandboxed,
        Err(why) => {
            return Ok(CmdOutput::policy("refused", why.clone())
                .field("refused_kind", json!("config"))
                .field("reason", json!(why)));
        }
    };
    let repo = repo_root(ctx)?;
    let mut cfg = load_config(args, ctx, &repo)?;
    ctx.apply_assigned_account_executor(&repo, &mut cfg);
    if cfg
        .repo
        .cross_audit
        .executor
        .trim()
        .eq_ignore_ascii_case("auto")
    {
        let resolved = resolve_auto_audit_executor(
            &cfg.repo.run.executor,
            &cfg.repo.cross_audit.executor,
            &ctx.path_var,
            &ctx.home,
        );
        if resolved.eq_ignore_ascii_case(cfg.repo.run.executor.trim())
            && cfg.repo.cross_audit.executor_cmd.trim().is_empty()
            && cfg.repo.cross_audit.model.trim().is_empty()
            && cfg.repo.cross_audit.effort.trim().is_empty()
        {
            cfg.repo.cross_audit.executor.clear();
        } else {
            cfg.repo.cross_audit.executor = resolved;
        }
    }
    let fallback = aaf_engine::factory::fall_back_to_one_provider(&mut cfg.repo);
    cfg.warnings.extend(fallback);
    let run_exec = cfg.repo.run.executor.trim();
    let audit_exec = configured_audit_provider(run_exec, &cfg.repo.cross_audit);
    if cfg.repo.cross_audit.require_different_provider && same_audit_provider(run_exec, audit_exec)
    {
        let why = format!(
            "cross_audit.require_different_provider is true, but effective auditor ({audit_exec}) \
             matches [run].executor ({run_exec}); configure a different [cross_audit].executor"
        );
        return Ok(CmdOutput::policy("refused", why.clone())
            .field("refused_kind", json!("config"))
            .field("reason", json!(why))
            .field("frozen", json!(false)));
    }
    let db = open_db(ctx)?;
    if factory::is_frozen(&db).map_err(|e| err("store", e))? {
        let text = "factory is frozen after repeated failures: aaf board unfreeze";
        return Ok(CmdOutput::policy("frozen", text).field("frozen", json!(true)));
    }
    let Some(lock) = run_lock(ctx, &repo)? else {
        let text = "another aaf run is already working on this repository";
        return Ok(CmdOutput::policy("refused", text)
            .field("refused_kind", json!("busy"))
            .field("reason", json!(text)));
    };

    let created_task_id = if let Some(raw_goal) = &args.goal {
        match goal::create_goal_task(
            raw_goal,
            &args.accept,
            args.priority.as_deref(),
            &args.scope,
            &repo,
            &db,
        )? {
            goal::GoalOutcome::Created(id) => Some(id),
            goal::GoalOutcome::Refused(out) => return Ok(out),
        }
    } else {
        None
    };

    if args.detach && !args.detached_child {
        let needs_claude_hook = run_executor_is_claude(&cfg.repo.run)
            || any_stage_uses_claude(&cfg.repo)
            || audit_exec.eq_ignore_ascii_case("claude");
        let writer_execs = writer_hook_executors(&cfg.repo);
        if (needs_claude_hook || !writer_execs.is_empty())
            && (created_task_id.is_some() || has_work_in_db(&db, &repo)?)
        {
            let base =
                resolve_base(&repo, &cfg.repo.gate.base_ref).map_err(|e| err("base branch", e))?;
            let base_sha =
                aaf_engine::git::cmd::git(&repo, &["rev-parse", "--verify", &base.start_point()])
                    .map_err(|e| err("base branch", e))?;
            if let Err(problem) = deps::check_base_hooks(
                &repo,
                &base_sha,
                &ctx.path_var,
                needs_claude_hook,
                &writer_execs,
            ) {
                return Ok(CmdOutput::policy("refused", problem.clone())
                    .field("refused_kind", json!("config"))
                    .field("reason", json!(problem))
                    .field("frozen", json!(false)));
            }
        }
        // Release the probed lock so the detached child acquires it immediately.
        drop(lock);
        return detach::spawn_detached(args, ctx, &repo, created_task_id.as_deref());
    }
    let _lock = lock;
    let stop_guard = stop::StopFileGuard::arm(&ctx.data_dir, &repo)?;
    let should_stop = || stop_guard.should_stop();
    let meta_task_id = created_task_id
        .clone()
        .or_else(|| std::env::var(detach::DETACHED_TASK_ENV).ok());
    let meta_log = args
        .detached_child
        .then(|| detach::detached_log_path(&ctx.data_dir, &repo));
    detach::write_run_meta(
        &ctx.data_dir,
        &repo,
        meta_log.as_deref(),
        meta_task_id.as_deref(),
    )?;

    let (mut deps, pool) = build_deps(ctx, &repo, &cfg, &db, sandboxed, args.max_parallel)?;
    db.sync_repo_budget_limits(&deps.settings.repo_root, &cfg.repo.unblocker)
        .map_err(|e| err("budget sync", e))?;
    let router = stage_wiring::build_stage_router(
        ctx,
        &repo,
        &cfg,
        &db,
        Arc::clone(&deps.runner),
        Arc::clone(&deps.accounts),
    )?;
    if router.default_implementer.provider_kind == Some(ProviderKind::Antigravity) {
        deps.accounts = Arc::clone(&router.default_implementer.accounts);
    }
    let audit = stage_wiring::build_native_audit_backend(ctx, &repo, &cfg, &db, Some(&router))?;
    let needs_claude_hook = run_executor_is_claude(&cfg.repo.run)
        || any_stage_uses_claude(&cfg.repo)
        || audit.as_ref().is_some_and(|a| a.hook_required);
    let writer_execs = writer_hook_executors(&cfg.repo);
    if (needs_claude_hook || !writer_execs.is_empty()) && has_work(&deps)? {
        // Once, before any task is taken: a registration that is wrong on
        // the base is wrong for every task alike. Blocking them one by one
        // counted each as a failed root and froze the factory for every
        // repository over one configuration problem. With nothing to start
        // there is nothing to refuse; a task that turns up later (`--auto`)
        // is still checked in its own worktree.
        let base = deps.git.base_sha().map_err(|e| err("base branch", e))?;
        if let Err(problem) = deps::check_base_hooks(
            &repo,
            &base,
            &ctx.path_var,
            needs_claude_hook,
            &writer_execs,
        ) {
            return Ok(CmdOutput::policy("refused", problem.clone())
                .field("refused_kind", json!("config"))
                .field("reason", json!(problem))
                .field("frozen", json!(false)));
        }
    }
    let mut auto_split_count = 0_u32;
    if args.auto_split {
        let split_outcome = super::board::split::run_auto_split_with_stop(&deps, &should_stop)?;
        auto_split_count = split_outcome.split_count;
        if let Some(why) = split_outcome.halted {
            let summary = RunSummary {
                halted: Some(why),
                ..Default::default()
            };
            let binding = pool
                .binding(deps.clock.now())
                .map_err(|e| err("accounts", e))?;
            let mut out = report(&summary, &cfg.warnings, &binding)?
                .field("max_parallel", json!(deps.settings.max_parallel))
                .field("auto_split_count", json!(auto_split_count));
            if let Some(task_id) = &created_task_id {
                out = out.field("task_id", json!(task_id));
            }
            return Ok(out);
        }
    }
    // Owner issues become tasks without a person running import-issues;
    // `None` unless `[github]` is on and the allowlist has a trusted login.
    let issue_poller = std::sync::Mutex::new(if args.auto {
        aaf_engine::github_poll::IssuePoller::new(
            &repo,
            &cfg.global.safety.owner_github_allowlist,
            &cfg.repo.github,
        )
    } else {
        None
    });
    let gh = super::board::github::gh_runner();
    let exported_reports = std::sync::Mutex::new(std::collections::HashMap::<String, i64>::new());
    let repo_root_str = deps.settings.repo_root.clone();
    let hooks = Hooks {
        idle: &|| {
            if let Ok(mut seen) = exported_reports.lock() {
                report::flush_completed_reports_for_repo(
                    ctx,
                    &db,
                    &repo_root_str,
                    &cfg.repo,
                    &mut seen,
                );
            }
            if should_stop() {
                return false;
            }
            if let Ok(mut guard) = issue_poller.lock()
                && let Some(poller) = guard.as_mut()
                && let Some(e) = poller.tick(&gh, &db, deps.clock.now())
            {
                eprintln!("aaf: issue import: {e}");
            }
            full_gate_tick(&deps);
            !stop::sleep_or_stop(AUTO_POLL, &should_stop)
        },
        // What an interrupted run left alive still carries its AAF_RUN_ID or
        // sits in its sandbox; the marks of a run that never finished are
        // still on disk.
        reap: &|run_id| {
            let marks = executors::reaper::RunMarks::at(&ctx.data_dir.join("marks"), run_id)?;
            let found = std::path::Path::new(&marks.mark).exists();
            executors::reaper::reap_in(run_id, found.then_some(&marks))?;
            if found {
                marks.remove()?;
            }
            Ok(())
        },
    };
    let mode = if args.auto {
        Mode::Auto
    } else if args.once {
        Mode::Once
    } else {
        Mode::Drain
    };
    let summary = if should_stop() {
        RunSummary::default()
    } else {
        factory::run_with_router_and_stop(
            &deps,
            audit.as_ref(),
            Some(&router),
            mode,
            &hooks,
            &should_stop,
        )
        .map_err(|e| err("factory", e))?
    };
    // Read after the run: the binding of a repository's first run exists
    // only once that run is recorded.
    let binding = pool
        .binding(deps.clock.now())
        .map_err(|e| err("accounts", e))?;
    let mut out = report(&summary, &cfg.warnings, &binding)?
        .field("max_parallel", json!(deps.settings.max_parallel))
        .field("auto_split_count", json!(auto_split_count));
    out = attach_auto_handover_reports(out, &summary, ctx, &db, &cfg.repo);
    out = stop::apply_stopped_report(out, stop_guard.was_stopped(), &summary);
    if summary.halted.is_some() {
        out = out.field("halt_recorded", json!(true));
    }
    if let Some(task_id) = &created_task_id {
        out = out.field("task_id", json!(task_id));
    }
    if auto_split_count > 0 {
        out.text
            .push_str(&format!("\nauto-split: {auto_split_count} root task(s)"));
    }
    Ok(out)
}

/// The idle full gate on the base branch (`[full_gate]`); only what it did
/// is printed, a skip is silent.
fn full_gate_tick(deps: &aaf_engine::factory::Deps) {
    use aaf_engine::factory::full_gate::{self, Outcome};
    match full_gate::tick(deps) {
        Ok(Outcome::Skipped(_)) => {}
        Ok(Outcome::DocsOnly { sha }) => eprintln!("aaf: full gate: {sha} changed only docs"),
        Ok(Outcome::Green { sha }) => eprintln!("aaf: full gate: {sha} is green"),
        Ok(Outcome::Flaky { sha, task_id }) => {
            eprintln!("aaf: full gate: {sha} has a flaky test, task {task_id}")
        }
        Ok(Outcome::Unverified { sha, reasons }) => eprintln!(
            "aaf: full gate: {sha} could not be verified: {}",
            reasons.join("; ")
        ),
        Ok(Outcome::Red { sha, task_id }) => eprintln!(
            "aaf: full gate: {sha} is red, task {task_id} opened; auto-merge held until green"
        ),
        Err(e) => eprintln!("aaf: full gate: {e}"),
    }
}
