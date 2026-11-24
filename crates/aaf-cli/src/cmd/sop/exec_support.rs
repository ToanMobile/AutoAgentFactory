//! Shared sandbox, executor, and account acquisition helpers for `aaf sop` and `aaf plan`.

use std::path::Path;
use std::sync::Arc;

use aaf_domain::account::AccountProfile;
use aaf_domain::config::RunConfig;
use aaf_engine::accounts::{self, DiscoveryEnv, Router};
use aaf_engine::config::EffectiveConfig;
use aaf_engine::executors::{self, ExecEnv};
use aaf_engine::factory::adapters::{RouterPool, provider_for};
use aaf_engine::factory::{AccountPool, Grant, Purpose, SelectingPool};

use super::super::board::store::open_db;
use crate::ctx::{Ctx, SystemClock};

pub(crate) fn host_sandbox_available() -> bool {
    cfg!(target_os = "macos")
        || (cfg!(target_os = "linux")
            && executors::linux_sandbox::detect_host_linux_isolation().is_isolated())
}

pub(crate) fn build_exec_env(ctx: &Ctx, cfg: &EffectiveConfig) -> ExecEnv {
    ExecEnv {
        home: Some(ctx.home.to_string_lossy().into_owned()),
        claude_bin: executors::find_on_path(&ctx.path_var, "claude")
            .map(|p| p.to_string_lossy().into_owned()),
        agy_bin: executors::agy::AgyRunner::resolve_binary(&ctx.path_var)
            .map(|p| p.to_string_lossy().into_owned()),
        data_dir: Some(ctx.data_dir.to_string_lossy().into_owned()),
        worktrees_root: Some(ctx.worktrees_dir.to_string_lossy().into_owned()),
        machine_env_pass: cfg.global.run.env_pass.clone(),
        machine_writable_caches: cfg.global.machine.writable_caches.clone(),
        keychain_policy: cfg.global.safety.deny_macos_keychain,
        ..ExecEnv::from_process()
    }
}

pub(crate) fn build_stage_run_config(
    effective_exec: &str,
    effective_model: &str,
    read_only: bool,
    base_run: &RunConfig,
    ctx: &Ctx,
) -> Result<RunConfig, String> {
    let lower = effective_exec.to_ascii_lowercase();
    if lower == "claude" {
        return Ok(RunConfig {
            executor: "claude".to_string(),
            model: effective_model.to_string(),
            effort: base_run.effort.clone(),
            env_pass: base_run.env_pass.clone(),
            ..RunConfig::default()
        });
    }
    if lower == "antigravity" && base_run.executor_cmd.is_empty() {
        return Ok(RunConfig {
            executor: "antigravity".to_string(),
            model: effective_model.to_string(),
            effort: base_run.effort.clone(),
            env_pass: base_run.env_pass.clone(),
            ..RunConfig::default()
        });
    }
    if lower == "custom"
        || (lower == base_run.executor.trim().to_ascii_lowercase()
            && !base_run.executor_cmd.is_empty())
    {
        return Ok(RunConfig {
            executor: "custom".to_string(),
            executor_cmd: base_run.executor_cmd.clone(),
            env_pass: base_run.env_pass.clone(),
            ..RunConfig::default()
        });
    }
    let resolved = executors::registry::resolve_with_fallback(&lower, &[], &ctx.path_var, &[])?;
    let bin = resolved.binary_path.to_string_lossy().into_owned();
    let model_opt = (!effective_model.is_empty()).then_some(effective_model);
    let effort_opt = (!base_run.effort.trim().is_empty()).then_some(base_run.effort.as_str());
    let argv = executors::registry::build_headless_argv(
        resolved.descriptor,
        &bin,
        "{prompt}",
        model_opt,
        effort_opt,
        read_only,
    )?;
    Ok(RunConfig {
        executor: "custom".to_string(),
        executor_cmd: argv,
        env_pass: base_run.env_pass.clone(),
        ..RunConfig::default()
    })
}

pub(crate) type StageAccountLease = (Option<Grant>, Option<SelectingPool>, Vec<AccountProfile>);

pub(crate) fn acquire_stage_account(
    ctx: &Ctx,
    repo_root: &Path,
    cfg: &EffectiveConfig,
    effective_exec: &str,
    run_id: &str,
) -> Result<StageAccountLease, String> {
    let on_path = |name: &str| accounts::binary_on_path_in(&ctx.path_var, name);
    let found = accounts::discover(
        &cfg.global.accounts,
        &DiscoveryEnv {
            home: ctx.home.clone(),
            binary_on_path: &on_path,
        },
    );
    let Some(provider) = provider_for(effective_exec) else {
        return Ok((None, None, found));
    };
    if !found.iter().any(|a| a.provider == provider) {
        return Ok((None, None, found));
    }
    let db = open_db(ctx).map_err(|e| e.message)?;
    let clock = Arc::new(SystemClock);
    let leases_dir = ctx.data_dir.join("leases");
    let max_per_account = cfg.global.accounts.max_concurrent_per_account;
    Router::new(&db, &db, &*clock, leases_dir.clone(), max_per_account)
        .sync(&found)
        .map_err(|e| format!("accounts sync: {e}"))?;
    let pool = SelectingPool::new(
        RouterPool {
            accounts: Arc::new(db.clone()),
            runs: Arc::new(db.clone()),
            clock,
            leases_dir,
            max_concurrent_per_account: max_per_account,
            provider: Some(provider),
        },
        cfg.global.accounts.selection,
        repo_root,
        Arc::new(db),
    );
    let all_profiles = pool.profiles().unwrap_or(found);
    let grant = pool
        .acquire_for(run_id, Purpose::Work)
        .map_err(|e| format!("account acquire: {e}"))?
        .ok_or_else(|| format!("no account is free for executor {effective_exec:?}"))?;
    Ok((Some(grant), Some(pool), all_profiles))
}
