//! Dependency wiring (`Deps`, `SelectingPool`) for `aaf run`.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;

use aaf_domain::account::ProviderKind;
use aaf_domain::config::{AccountSelection, RunConfig};
use aaf_domain::ports::{AccountStore, AgentRunner, Clock, TaskStore};
use aaf_engine::accounts::{self, DiscoveryEnv, Router};
use aaf_engine::config::{self, EffectiveConfig};
use aaf_engine::db::Db;
use aaf_engine::executors::{self, ExecEnv};
use aaf_engine::factory::adapters::{
    RealGate, RouterPool, SystemClock, WebhookNotifier, provider_for,
};
use aaf_engine::factory::stage_router::writer_requires_hook;
use aaf_engine::factory::{Deps, RealGit, SelectingPool, Settings, resolve_base};
use aaf_engine::hook_settings;
use aaf_engine::split::{apply_multi_repo_fair_share, compute_auto_parallel};
use aaf_engine::throttle::{LoadProbe, SystemProbe};

use super::super::board::store::err;
use super::{ParallelArg, progress};
use crate::ctx::Ctx;
use crate::output::{CmdError, notice};

const MOBILE_PROFILES: [&str; 5] = ["android", "ios", "flutter", "react-native", "expo"];

/// True when `[run]` invokes Claude Code natively or via `run.executor_cmd`.
pub(crate) fn run_executor_is_claude(run: &RunConfig) -> bool {
    aaf_domain::pipeline::normalize_executor_id(&run.executor) == "claude"
        || run
            .executor_cmd
            .first()
            .and_then(|arg0| Path::new(arg0.trim()).file_name())
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("claude"))
}

fn executor_requires_hook(run: &RunConfig) -> bool {
    run_executor_is_claude(run) || writer_requires_hook(&run.executor, &run.executor_cmd.join(" "))
}

pub(super) fn check_base_hooks(
    repo: &Path,
    base_sha: &str,
    path_var: &OsStr,
    needs_claude_hook: bool,
    writer_execs: &BTreeSet<String>,
) -> Result<(), String> {
    if needs_claude_hook {
        hook_settings::check_base(repo, base_sha, path_var)?;
    }
    for exec in writer_execs {
        hook_settings::check_base_for_executor(repo, base_sha, path_var, exec)?;
    }
    Ok(())
}

pub(super) fn build_deps(
    ctx: &Ctx,
    repo: &Path,
    cfg: &EffectiveConfig,
    db: &Db,
    sandboxed: bool,
    parallel_arg: Option<ParallelArg>,
) -> Result<(Deps, Arc<SelectingPool>), CmdError> {
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
    // R3-CONC-5: Antigravity is strictly serialized to 1 slot per account.
    let eff_max_per_account = if run_provider == Some(ProviderKind::Antigravity) {
        1
    } else {
        max_per_account
    };
    // `run.max_parallel = 0` (the default) means the same as `--max-parallel auto`.
    let parallel_arg = match parallel_arg {
        None if cfg.repo.run.max_parallel == 0 => Some(ParallelArg::Auto),
        other => other,
    };
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
            let base_auto = compute_auto_parallel(
                provider_accounts,
                eff_max_per_account,
                cores,
                SystemProbe::detect().pressure(),
            );
            let ready_slots =
                count_ready_provider_slots(db, run_provider, eff_max_per_account, clock.now());
            let active_repos = count_active_repos_for_fair_share(db, &ctx.data_dir, repo);
            let fair = apply_multi_repo_fair_share(base_auto, ready_slots, active_repos);
            if run_provider == Some(ProviderKind::Antigravity) {
                fair.min(1)
            } else {
                fair
            }
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
        keychain_policy: cfg.global.safety.deny_macos_keychain,
        ..ExecEnv::from_process()
    };
    let runner: Arc<dyn AgentRunner> = Arc::from(
        executors::build_runner_with(&cfg.repo.run, exec_env).map_err(|e| err("executor", e))?,
    );
    let base = resolve_base(repo, &cfg.repo.gate.base_ref).map_err(|e| err("base branch", e))?;
    let profile = config::effective_profile(&cfg.repo, repo);
    // Which account a job may take follows `accounts.selection`; `repo` is
    // the path `tasks.repo_root` holds, so the run history finds it.
    // Always attach live cross-repo gating audit reserve so every repository
    // sharing the machine account pool cooperates under `_reserve.lock`.
    let selecting = SelectingPool::new(
        RouterPool {
            accounts: Arc::new(db.clone()),
            runs: Arc::new(db.clone()),
            clock: clock.clone(),
            leases_dir,
            max_concurrent_per_account: eff_max_per_account,
            provider: provider_for(&cfg.repo.run.executor),
        },
        cfg.global.accounts.selection,
        repo,
        Arc::new(db.clone()),
    )
    .with_gating_audit_reserve(super::audit_reserve::gating_authors_provider(
        ctx,
        repo,
        cfg,
        db.clone(),
    ));
    let pool = Arc::new(selecting);

    let deps = Deps {
        store: Arc::new(db.clone()),
        clock: clock.clone(),
        runner,
        accounts: pool.clone(),
        git: Arc::new(
            RealGit::new(
                repo.to_path_buf(),
                ctx.worktrees_dir.clone(),
                ctx.data_dir.join("locks"),
                base,
            )
            .with_cwd(ctx.cwd.clone()),
        ),
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
            hook_required: executor_requires_hook(&cfg.repo.run),
            // The executor hands the PATH of this process on to the agent.
            agent_path: ctx.path_var.clone(),
            mobile: MOBILE_PROFILES.contains(&profile.as_str()),
            data_dir: ctx.data_dir.clone(),
            home: ctx.home.clone(),
            owner_github_allowlist: cfg.global.safety.owner_github_allowlist.clone(),
        },
    };
    Ok((deps, pool))
}

fn count_ready_provider_slots(
    db: &Db,
    provider: Option<ProviderKind>,
    max_per_account: u32,
    now: i64,
) -> usize {
    let Some(kind) = provider else {
        return 0;
    };
    let ready = db
        .list_accounts()
        .unwrap_or_default()
        .into_iter()
        .filter(|state| {
            state.profile.provider == kind
                && !state.in_cooldown(now)
                && Path::new(&state.profile.config_dir).is_dir()
        })
        .count();
    ready.saturating_mul(max_per_account.max(1) as usize)
}

fn supervisor_allowed_repos(data_dir: &Path) -> Option<BTreeSet<String>> {
    let raw = std::fs::read_to_string(data_dir.join("ui-supervisor.json")).ok()?;
    let val: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let arr = val.get("allowed_repos")?.as_array()?;
    let set = arr
        .iter()
        .filter_map(|v| v.as_str())
        .map(|s| normalize_repo_path(s.trim()))
        .filter(|s| !s.is_empty())
        .collect::<BTreeSet<_>>();
    Some(set)
}

fn normalize_repo_path(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    let p = Path::new(raw);
    if p.is_absolute() {
        std::fs::canonicalize(p)
            .unwrap_or_else(|_| p.to_path_buf())
            .to_string_lossy()
            .into_owned()
    } else {
        raw.to_string()
    }
}

fn repo_is_allowed(allowed: Option<&BTreeSet<String>>, repo_root: &str) -> bool {
    let Some(set) = allowed else {
        return true;
    };
    let canon = normalize_repo_path(repo_root);
    if set.contains(repo_root) || set.contains(&canon) {
        return true;
    }
    Path::new(repo_root)
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| set.contains(name))
}

fn count_active_repos_for_fair_share(db: &Db, data_dir: &Path, current_repo: &Path) -> usize {
    let allowed = supervisor_allowed_repos(data_dir);
    let mut repos = BTreeSet::new();
    repos.insert(normalize_repo_path(&current_repo.to_string_lossy()));
    // Adopting counts: the adopt lane gates and merges its PR on this repo's share.
    for state in super::WORK_STATES {
        for task in db.list_tasks(None, Some(state)).unwrap_or_default() {
            if Path::new(&task.repo_root).is_dir()
                && repo_is_allowed(allowed.as_ref(), &task.repo_root)
            {
                repos.insert(normalize_repo_path(&task.repo_root));
            }
        }
    }
    repos.len().max(1)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use aaf_domain::ports::TaskStore;
    use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};

    use super::*;

    fn insert(db: &Db, id: &str, repo: &Path, state: TaskState) {
        let spec = ImmutableSpec {
            description: "spec".into(),
            acceptance: vec!["ok".into()],
        };
        let mut task = Task::new_root(id, repo.to_string_lossy(), id, spec, Priority::P2, 10);
        task.state = state;
        db.insert_task(&task).unwrap();
    }

    #[test]
    fn fair_share_counts_a_repository_whose_only_work_is_an_adopting_pr() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
        let (a, b, c) = (
            tmp.path().join("a"),
            tmp.path().join("b"),
            tmp.path().join("c"),
        );
        for dir in [&a, &b, &c] {
            std::fs::create_dir_all(dir).unwrap();
        }
        insert(&db, "t-a", &a, TaskState::Ready);
        insert(&db, "t-done", &c, TaskState::Done);
        assert_eq!(count_active_repos_for_fair_share(&db, tmp.path(), &a), 1);

        insert(&db, "t-ad", &b, TaskState::Adopting);
        assert_eq!(
            count_active_repos_for_fair_share(&db, tmp.path(), &a),
            2,
            "the adopt lane is work on repository b"
        );
    }
}
