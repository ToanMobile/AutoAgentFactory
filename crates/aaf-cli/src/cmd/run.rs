//! `aaf run [--auto] [--auto-merge] [--auto-split] [--max-parallel N|auto] [--unsandboxed]`
//! (docs/PLAN.md C.10): wires the real store, executor, git and gate into
//! the factory loop of `aaf_engine::factory`.

mod progress;
#[cfg(test)]
mod tests;

use std::fs::{File, TryLockError};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use aaf_domain::config::{AccountSelection, CrossAuditConfig, RunConfig};
use aaf_domain::ports::AgentRunner;
use aaf_domain::task::TaskState;
use aaf_engine::accounts::{self, DiscoveryEnv, Router};
use aaf_engine::config::{self, ConfigInputs, ConfigOverlay, EffectiveConfig, RunOverlay};
use aaf_engine::db::{Db, private};
use aaf_engine::executors::{self, ExecEnv};
use aaf_engine::factory::adapters::{
    RealGate, RouterPool, SystemClock, WebhookNotifier, provider_for,
};
use aaf_engine::factory::{
    self, AccountPool, AuditBackend, BindingView, Deps, Hooks, Mode, RealGit, RunSummary,
    SelectingPool, Settings, build_audit_command, configured_audit_provider, resolve_base,
    same_audit_provider,
};
use aaf_engine::hook_settings;
use aaf_engine::split::compute_auto_parallel;
use aaf_engine::throttle::{LoadProbe, SystemProbe};
use serde_json::{Value, json};

use super::board::store::{err, open_db, refuse_inside_agent_run, repo_root};
use crate::ctx::Ctx;
use crate::output::{CmdError, CmdOutput, CmdResult, notice};

/// How often `--auto` looks for new Ready tasks and ended cooldowns.
const AUTO_POLL: Duration = Duration::from_secs(5);
const MOBILE_PROFILES: [&str; 5] = ["android", "ios", "flutter", "react-native", "expo"];

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

#[derive(Debug, clap::Args)]
pub struct RunArgs {
    /// Keep running: look for Ready tasks until interrupted or frozen.
    #[arg(long)]
    pub auto: bool,
    /// Land clear tasks through the merge queue instead of opening a PR.
    #[arg(long)]
    pub auto_merge: bool,
    /// Agent runs at the same time (default: `run.max_parallel`; or `auto`).
    #[arg(long, value_name = "N|auto")]
    pub max_parallel: Option<ParallelArg>,
    /// Proactively split Ready root tasks into 2..=4 disjoint subtasks before running.
    #[arg(long)]
    pub auto_split: bool,
    /// Run agents without the Seatbelt sandbox. Required where there is none.
    #[arg(long)]
    pub unsandboxed: bool,
}

/// Whether agent runs are sandboxed. Seatbelt exists on macOS only; anywhere
/// else running unconfined has to be asked for by name (PLAN A.17).
pub(crate) fn sandbox_choice(unsandboxed: bool, macos: bool) -> Result<bool, &'static str> {
    match (unsandboxed, macos) {
        (true, _) => Ok(false),
        (false, true) => Ok(true),
        (false, false) => {
            Err("no agent sandbox on this platform: pass --unsandboxed to run agents unconfined")
        }
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
    let path = dir.join(format!("{}.run.lock", aaf_fp::repo_id(repo)));
    let file = private::open_file(&path).map_err(|e| err(&path.display().to_string(), e))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(err(&path.display().to_string(), e)),
    }
}

fn build_audit_backend(
    repo: &Path,
    cfg: &EffectiveConfig,
    db: &Db,
    clock: Arc<SystemClock>,
    leases_dir: std::path::PathBuf,
    exec_env: ExecEnv,
) -> Result<Option<AuditBackend>, CmdError> {
    let cross = &cfg.repo.cross_audit;
    if cross.executor.trim().is_empty()
        && cross.executor_cmd.trim().is_empty()
        && cross.model.trim().is_empty()
        && cross.effort.trim().is_empty()
    {
        return Ok(None);
    }
    let audit_exec = configured_audit_provider(&cfg.repo.run.executor, cross);
    let is_native_claude =
        audit_exec.eq_ignore_ascii_case("claude") && cross.executor_cmd.trim().is_empty();
    let audit_run_cfg = if is_native_claude {
        RunConfig {
            executor: "claude".to_string(),
            model: if cross.model.trim().is_empty() {
                cfg.repo.run.model.clone()
            } else {
                cross.model.clone()
            },
            effort: if cross.effort.trim().is_empty() {
                cfg.repo.run.effort.clone()
            } else {
                cross.effort.clone()
            },
            env_pass: cfg.repo.run.env_pass.clone(),
            ..RunConfig::default()
        }
    } else {
        let effective = CrossAuditConfig {
            executor: audit_exec.to_string(),
            ..cross.clone()
        };
        let cmd = build_audit_command(&effective).map_err(|e| err("cross_audit", e))?;
        RunConfig {
            executor: "custom".to_string(),
            executor_cmd: cmd,
            env_pass: cfg.repo.run.env_pass.clone(),
            ..RunConfig::default()
        }
    };
    let runner: Arc<dyn AgentRunner> = Arc::from(
        executors::build_runner_with(&audit_run_cfg, exec_env)
            .map_err(|e| err("cross_audit executor", e))?,
    );
    let accounts: Arc<dyn AccountPool> = Arc::new(SelectingPool::new(
        RouterPool {
            accounts: Arc::new(db.clone()),
            runs: Arc::new(db.clone()),
            clock,
            leases_dir,
            max_concurrent_per_account: cfg.global.accounts.max_concurrent_per_account,
            provider: provider_for(audit_exec),
        },
        cfg.global.accounts.selection,
        repo,
        Arc::new(db.clone()),
    ));
    Ok(Some(AuditBackend {
        provider: audit_exec.to_string(),
        runner,
        accounts,
        hook_required: is_native_claude || audit_exec.eq_ignore_ascii_case("claude"),
    }))
}

fn build_deps(
    ctx: &Ctx,
    repo: &Path,
    cfg: &EffectiveConfig,
    db: &Db,
    sandboxed: bool,
    parallel_arg: Option<ParallelArg>,
) -> Result<(Deps, Arc<SelectingPool>, Option<AuditBackend>), CmdError> {
    let clock = Arc::new(SystemClock);
    let leases_dir = ctx.data_dir.join("leases");
    let max_per_account = cfg.global.accounts.max_concurrent_per_account;

    let on_path = |name: &str| accounts::binary_on_path_in(&ctx.path_var, name);
    let found = accounts::discover(
        &cfg.global.accounts,
        &DiscoveryEnv {
            home: ctx.home.clone(),
            binary_on_path: &on_path,
        },
    );
    Router::new(db, db, &*clock, leases_dir.clone(), max_per_account)
        .sync(&found)
        .map_err(|e| err("accounts", e))?;

    let run_provider = provider_for(&cfg.repo.run.executor);
    let effective_max_parallel = match parallel_arg {
        Some(ParallelArg::Auto) => {
            let cores = std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(2);
            let provider_accounts = match run_provider {
                Some(kind) => {
                    let count = found.iter().filter(|a| a.provider == kind).count();
                    if cfg.global.accounts.selection == AccountSelection::Pinned {
                        count.min(1)
                    } else {
                        count
                    }
                }
                None => 0,
            };
            compute_auto_parallel(
                provider_accounts,
                max_per_account,
                cores,
                SystemProbe::detect().pressure(),
            )
        }
        Some(ParallelArg::Fixed(n)) => n,
        None => cfg.repo.run.max_parallel,
    };

    // The sandbox must close the data dir and open exactly the worktree
    // directory this command creates worktrees in.
    let exec_env = ExecEnv {
        data_dir: Some(ctx.data_dir.to_string_lossy().into_owned()),
        worktrees_root: Some(ctx.worktrees_dir.to_string_lossy().into_owned()),
        // The operator's own list (tier 1), kept apart: only it may name a
        // credential or NODE_OPTIONS. The executor adds the repository's
        // `run.env_pass` (tier 2) to `env_pass`.
        machine_env_pass: cfg.global.run.env_pass.clone(),
        ..ExecEnv::from_process()
    };
    let runner: Arc<dyn AgentRunner> = Arc::from(
        executors::build_runner_with(&cfg.repo.run, exec_env.clone())
            .map_err(|e| err("executor", e))?,
    );
    let audit = build_audit_backend(repo, cfg, db, clock.clone(), leases_dir.clone(), exec_env)?;
    let base = resolve_base(repo, &cfg.repo.gate.base_ref).map_err(|e| err("base branch", e))?;
    let profile = config::effective_profile(&cfg.repo, repo);
    // Which account a job may take follows `accounts.selection`; `repo` is
    // the path `tasks.repo_root` holds, so the run history finds it.
    let pool = Arc::new(SelectingPool::new(
        RouterPool {
            accounts: Arc::new(db.clone()),
            runs: Arc::new(db.clone()),
            clock: clock.clone(),
            leases_dir,
            max_concurrent_per_account: max_per_account,
            provider: provider_for(&cfg.repo.run.executor),
        },
        cfg.global.accounts.selection,
        repo,
        Arc::new(db.clone()),
    ));

    let deps = Deps {
        store: Arc::new(db.clone()),
        clock: clock.clone(),
        runner,
        accounts: pool.clone(),
        git: Arc::new(RealGit::new(
            repo.to_path_buf(),
            ctx.worktrees_dir.clone(),
            ctx.data_dir.join("locks"),
            base,
        )),
        gate: Arc::new(RealGate {
            data_dir: ctx.data_dir.clone(),
            cfg: cfg.repo.clone(),
            clock,
            machine: cfg.global.machine.clone(),
            // A gate that waits for the machine says so, on stderr like
            // `aaf gate`; without it a throttled run looks hung.
            progress: Box::new(|tree, event| notice("run", &progress::line(tree, event))),
            worktrees_dir: ctx.worktrees_dir.clone(),
        }),
        notifier: Arc::new(WebhookNotifier {
            url: cfg.global.notifications.webhook_url.clone(),
        }),
        settings: Settings {
            repo_root: repo.to_string_lossy().into_owned(),
            repo: cfg.repo.clone(),
            max_parallel: effective_max_parallel,
            auto_merge: cfg.repo.run.auto_merge,
            sandboxed,
            hook_required: cfg.repo.run.executor == "claude",
            // The executor hands the PATH of this process on to the agent.
            agent_path: ctx.path_var.clone(),
            mobile: MOBILE_PROFILES.contains(&profile.as_str()),
            data_dir: ctx.data_dir.clone(),
            home: ctx.home.clone(),
        },
    };
    Ok((deps, pool, audit))
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

fn report(
    summary: &RunSummary,
    warnings: &[String],
    binding: &BindingView,
) -> Result<CmdOutput, CmdError> {
    let mut text = format!(
        "{} agent run(s), {} gate run(s): {} done, {} in review, {} blocked, {} back to ready, \
         {} waiting for an account{}",
        summary.agent_runs,
        summary.gate_runs,
        summary.done.len(),
        summary.review.len(),
        summary.blocked.len(),
        summary.requeued.len(),
        summary.waiting_for_account,
        if summary.frozen {
            "; factory is FROZEN: aaf board unfreeze"
        } else {
            ""
        }
    );
    if summary.waiting_for_review_cap > 0 {
        text.push_str(&format!(
            "\nwaiting for review cap: {} (run.max_open_review reached)",
            summary.waiting_for_review_cap
        ));
    }
    if !summary.blocked_upstream.is_empty() {
        let pairs: Vec<String> = summary
            .blocked_upstream
            .iter()
            .map(|(task, blocker)| format!("{task} <- {blocker}"))
            .collect();
        text.push_str(&format!("\nblocked upstream: {}", pairs.join(", ")));
    }
    if let Some(line) = waiting_line(summary.waiting_for_account, binding) {
        text.push('\n');
        text.push_str(&line);
    }
    // PLAN C.10: 2 = Blocked. A task that ended Blocked needs a human, and
    // every blocking error blocks its task; what is left in `errors` beside
    // that (a pull request that could not be opened, a leftover worktree)
    // stopped nothing and keeps exit 0.
    // A halted run needs a person too (a login, the network), and its task
    // is in Ready, not Blocked: said apart so it is not taken for an idle
    // board.
    let mut out = if summary.frozen {
        CmdOutput::policy("frozen", text)
    } else if let Some(why) = &summary.halted {
        CmdOutput::policy("halted", format!("{text}\nstopped: {why}"))
    } else if !summary.blocked.is_empty() {
        let ids = summary.blocked.join(", ");
        CmdOutput::policy(
            "blocked",
            format!("{text}\nblocked: {ids} (aaf board show <id> says why)"),
        )
    } else {
        CmdOutput::ok("ok", text)
    };
    let Value::Object(fields) = serde_json::to_value(summary).map_err(|e| err("summary", e))?
    else {
        return Err(CmdError::new("summary is not an object"));
    };
    out.fields.extend(fields);
    Ok(out
        .field("warnings", json!(warnings))
        .field("account_binding", json!(binding)))
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
        return Ok(refused);
    }
    let sandboxed = match sandbox_choice(args.unsandboxed, cfg!(target_os = "macos")) {
        Ok(sandboxed) => sandboxed,
        Err(why) => return Ok(CmdOutput::policy("refused", why)),
    };
    let repo = repo_root(ctx)?;
    let cfg = load_config(args, ctx, &repo)?;
    let run_exec = cfg.repo.run.executor.trim();
    let audit_exec = configured_audit_provider(run_exec, &cfg.repo.cross_audit);
    if cfg.repo.cross_audit.require_different_provider && same_audit_provider(run_exec, audit_exec)
    {
        let why = format!(
            "cross_audit.require_different_provider is true, but effective auditor ({audit_exec}) \
             matches [run].executor ({run_exec}); configure a different [cross_audit].executor"
        );
        return Ok(CmdOutput::policy("refused", why.clone())
            .field("reason", json!(why))
            .field("frozen", json!(false)));
    }
    let db = open_db(ctx)?;
    if factory::is_frozen(&db).map_err(|e| err("store", e))? {
        let text = "factory is frozen after repeated failures: aaf board unfreeze";
        return Ok(CmdOutput::policy("frozen", text).field("frozen", json!(true)));
    }
    let Some(_lock) = run_lock(ctx, &repo)? else {
        let text = "another aaf run is already working on this repository";
        return Ok(CmdOutput::policy("refused", text));
    };

    let (deps, pool, audit) = build_deps(ctx, &repo, &cfg, &db, sandboxed, args.max_parallel)?;
    let needs_hook = deps.settings.hook_required || audit.as_ref().is_some_and(|a| a.hook_required);
    if needs_hook && has_work(&deps)? {
        // Once, before any task is taken: a registration that is wrong on
        // the base is wrong for every task alike. Blocking them one by one
        // counted each as a failed root and froze the factory for every
        // repository over one configuration problem. With nothing to start
        // there is nothing to refuse; a task that turns up later (`--auto`)
        // is still checked in its own worktree.
        let base = deps.git.base_sha().map_err(|e| err("base branch", e))?;
        if let Err(problem) = hook_settings::check_base(&repo, &base, &ctx.path_var) {
            return Ok(CmdOutput::policy("refused", problem.clone())
                .field("reason", json!(problem))
                .field("frozen", json!(false)));
        }
    }
    let mut auto_split_count = 0_u32;
    if args.auto_split {
        let split_outcome = super::board::split::run_auto_split(&deps)?;
        auto_split_count = split_outcome.split_count;
        if let Some(why) = split_outcome.halted {
            let summary = RunSummary {
                halted: Some(why),
                ..Default::default()
            };
            let binding = pool
                .binding(deps.clock.now())
                .map_err(|e| err("accounts", e))?;
            return Ok(report(&summary, &cfg.warnings, &binding)?
                .field("max_parallel", json!(deps.settings.max_parallel))
                .field("auto_split_count", json!(auto_split_count)));
        }
    }
    let hooks = Hooks {
        idle: &|| {
            std::thread::sleep(AUTO_POLL);
            true
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
    let mode = if args.auto { Mode::Auto } else { Mode::Once };
    let summary = factory::run_with_audit(&deps, audit.as_ref(), mode, &hooks)
        .map_err(|e| err("factory", e))?;
    // Read after the run: the binding of a repository's first run exists
    // only once that run is recorded.
    let binding = pool
        .binding(deps.clock.now())
        .map_err(|e| err("accounts", e))?;
    let mut out = report(&summary, &cfg.warnings, &binding)?
        .field("max_parallel", json!(deps.settings.max_parallel))
        .field("auto_split_count", json!(auto_split_count));
    if auto_split_count > 0 {
        out.text
            .push_str(&format!("\nauto-split: {auto_split_count} root task(s)"));
    }
    Ok(out)
}
