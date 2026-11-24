//! Dependency wiring (`Deps`, `SelectingPool`, `AuditBackend`) for `aaf run`.

use std::path::Path;
use std::sync::Arc;

use aaf_domain::config::{AccountSelection, CrossAuditConfig, RunConfig};
use aaf_domain::ports::AgentRunner;
use aaf_engine::accounts::{self, DiscoveryEnv, Router};
use aaf_engine::config::{self, EffectiveConfig};
use aaf_engine::db::Db;
use aaf_engine::executors::{self, ExecEnv};
use aaf_engine::factory::adapters::{
    RealGate, RouterPool, SystemClock, WebhookNotifier, provider_for,
};
use aaf_engine::factory::{
    AccountPool, AuditBackend, Deps, RealGit, SelectingPool, Settings, build_audit_command,
    configured_audit_provider, resolve_auto_audit_executor, resolve_base,
};
use aaf_engine::split::compute_auto_parallel;
use aaf_engine::throttle::{LoadProbe, SystemProbe};

use super::super::board::store::err;
use super::{ParallelArg, progress};
use crate::ctx::Ctx;
use crate::output::{CmdError, notice};

const MOBILE_PROFILES: [&str; 5] = ["android", "ios", "flutter", "react-native", "expo"];

fn build_audit_backend(
    ctx: &Ctx,
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
    let raw_audit_exec = configured_audit_provider(&cfg.repo.run.executor, cross);
    let resolved_audit_exec = resolve_auto_audit_executor(
        &cfg.repo.run.executor,
        raw_audit_exec,
        &ctx.path_var,
        &ctx.home,
    );
    let audit_exec = resolved_audit_exec.as_str();
    if cross.executor.trim().eq_ignore_ascii_case("auto")
        && audit_exec.eq_ignore_ascii_case(cfg.repo.run.executor.trim())
        && cross.executor_cmd.trim().is_empty()
        && cross.model.trim().is_empty()
        && cross.effort.trim().is_empty()
    {
        return Ok(None);
    }
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

pub(super) fn build_deps(
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
        machine_writable_caches: cfg.global.machine.writable_caches.clone(),
        ..ExecEnv::from_process()
    };
    let runner: Arc<dyn AgentRunner> = Arc::from(
        executors::build_runner_with(&cfg.repo.run, exec_env.clone())
            .map_err(|e| err("executor", e))?,
    );
    let audit = build_audit_backend(
        ctx,
        repo,
        cfg,
        db,
        clock.clone(),
        leases_dir.clone(),
        exec_env,
    )?;
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
