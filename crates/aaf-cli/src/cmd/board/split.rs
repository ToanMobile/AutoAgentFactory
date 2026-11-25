//! `aaf board split <id> [--max 2..=4] [--dry-run] [--unsandboxed]` and the
//! shared `--auto-split` pre-pass used by `aaf run`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use aaf_domain::ports::{
    AgentOutcome, AgentRequest, AgentRunner, Clock, InfraFailure, LimitHit, PortError, PortResult,
    TaskStore,
};
use aaf_domain::task::TaskState;
use aaf_engine::accounts::{self, DiscoveryEnv, Router};
use aaf_engine::executors::{self, ExecEnv};
use aaf_engine::factory::adapters::{RouterPool, SystemClock, provider_for};
use aaf_engine::factory::prompt::sandbox_policy_for;
use aaf_engine::factory::{
    AccountPool, Deps, GitOps, Purpose, RealGit, SelectingPool, is_frozen, resolve_base,
};
use aaf_engine::hook_settings;
use aaf_engine::split::{MAX_SPLIT_SUBTASKS, SplitOutcome, propose_and_apply_split};
use serde_json::json;

use super::port;
use super::store::{err, open_db, refuse_inside_agent_run, repo_root};
use crate::cmd::run::{run_executor_is_claude, sandbox_choice};
use crate::ctx::Ctx;
use crate::output::{CmdError, CmdOutput, CmdResult};

const SPLIT_TIMEOUT_SECS: u64 = 300;

#[derive(Debug, clap::Args)]
pub struct SplitArgs {
    /// Root task id to decompose into disjoint parallel subtasks.
    pub id: String,
    /// Maximum number of subtasks (2..=4, default 4).
    #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u64).range(2..=4))]
    pub max: u64,
    /// Print proposed subtasks without writing them to the database.
    #[arg(long)]
    pub dry_run: bool,
    /// Run the planner without Seatbelt sandbox (required on platforms without Seatbelt).
    #[arg(long)]
    pub unsandboxed: bool,
}

pub(crate) struct AutoSplitOutcome {
    pub split_count: u32,
    pub halted: Option<String>,
}

fn split_run_id(task_id: &str, now: i64) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let seed = format!("split:{task_id}:{now}:{}:{seq}", std::process::id());
    let hash = aaf_fp::sha256_hex(seed.as_bytes());
    format!("split-{}", &hash[..16])
}

/// Captures any [`InfraFailure`] or [`LimitHit`] reported by the inner runner
/// during a split planning call so `aaf run --auto-split` can halt or cool down
/// cleanly.
struct SplitInfraWatch<'a> {
    runner: &'a dyn AgentRunner,
    seen: Mutex<Option<InfraFailure>>,
    limit_hit: Mutex<Option<LimitHit>>,
}

impl<'a> SplitInfraWatch<'a> {
    fn new(runner: &'a dyn AgentRunner) -> Self {
        Self {
            runner,
            seen: Mutex::new(None),
            limit_hit: Mutex::new(None),
        }
    }

    fn take(self) -> (Option<InfraFailure>, Option<LimitHit>) {
        (
            self.seen
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner),
            self.limit_hit
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }
}

impl AgentRunner for SplitInfraWatch<'_> {
    fn run(&self, request: &AgentRequest) -> PortResult<AgentOutcome> {
        let outcome = self.runner.run(request)?;
        if let Some(infra) = &outcome.infra {
            *self.seen.lock().unwrap_or_else(PoisonError::into_inner) = Some(infra.clone());
        }
        if let Some(hit) = &outcome.limit_hit {
            *self
                .limit_hit
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(hit.clone());
        }
        Ok(outcome)
    }

    fn reports_tokens(&self) -> bool {
        self.runner.reports_tokens()
    }
}

fn format_outcome(outcome: &SplitOutcome) -> Result<CmdOutput, CmdError> {
    let mut lines = Vec::new();
    if outcome.split {
        let verb = if outcome.dry_run {
            "would split"
        } else {
            "split"
        };
        lines.push(format!(
            "{verb} {} into {} subtask(s)",
            outcome.parent_id,
            outcome.subtasks.len()
        ));
        for sub in &outcome.subtasks {
            if sub.owned_paths.is_empty() {
                lines.push(format!("  {}  {}", sub.id, sub.title));
            } else {
                lines.push(format!(
                    "  {}  {}  [{}]",
                    sub.id,
                    sub.title,
                    sub.owned_paths.join(", ")
                ));
            }
        }
    } else {
        let why = outcome
            .reason
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("planner did not split task");
        lines.push(format!("did not split {}: {why}", outcome.parent_id));
    }

    let subtasks = serde_json::to_value(&outcome.subtasks).map_err(|e| err("subtasks", e))?;
    Ok(CmdOutput::ok("ok", lines.join("\n"))
        .field("parent_id", json!(outcome.parent_id))
        .field("dry_run", json!(outcome.dry_run))
        .field("split", json!(outcome.split))
        .field("reason", json!(outcome.reason))
        .field("subtasks", subtasks))
}

pub fn run_split(args: &SplitArgs, ctx: &Ctx) -> CmdResult {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf board split") {
        return Ok(refused);
    }
    let sandboxed = match sandbox_choice(args.unsandboxed, cfg!(target_os = "macos")) {
        Ok(sandboxed) => sandboxed,
        Err(why) => return Ok(CmdOutput::policy("refused", why)),
    };
    let repo = repo_root(ctx)?;
    let cfg = ctx.load_config(&repo)?;
    let db = open_db(ctx)?;
    let task = db.get_task(&args.id).map_err(port)?;

    if task.depth != 0 {
        let text = format!(
            "{} is a subtask (depth {}); only root tasks can be split",
            task.id, task.depth
        );
        return Ok(CmdOutput::policy("refused", text)
            .field("task_id", json!(task.id))
            .field("depth", json!(task.depth)));
    }
    if !matches!(task.state, TaskState::Ready | TaskState::Backlog) {
        let text = format!(
            "{} is {}, not ready or backlog",
            task.id,
            task.state.as_str()
        );
        return Ok(CmdOutput::policy("refused", text)
            .field("task_id", json!(task.id))
            .field("state", json!(task.state)));
    }

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
    Router::new(&db, &db, &*clock, leases_dir.clone(), max_per_account)
        .sync(&found)
        .map_err(|e| err("accounts", e))?;

    let pool = SelectingPool::new(
        RouterPool {
            accounts: Arc::new(db.clone()),
            runs: Arc::new(db.clone()),
            clock: clock.clone(),
            leases_dir,
            max_concurrent_per_account: max_per_account,
            provider: provider_for(&cfg.repo.run.executor),
        },
        cfg.global.accounts.selection,
        &repo,
        Arc::new(db.clone()),
    );
    let Some(grant) = pool.acquire_for(&task.id, Purpose::Work).map_err(port)? else {
        let text = "no account is free to run the split planner";
        return Ok(CmdOutput::policy("refused", text).field("task_id", json!(task.id)));
    };
    let all_profiles = pool.profiles().map_err(port)?;

    let exec_env = ExecEnv {
        data_dir: Some(ctx.data_dir.to_string_lossy().into_owned()),
        worktrees_root: Some(ctx.worktrees_dir.to_string_lossy().into_owned()),
        machine_env_pass: cfg.global.run.env_pass.clone(),
        machine_writable_caches: cfg.global.machine.writable_caches.clone(),
        keychain_policy: cfg.global.safety.deny_macos_keychain,
        ..ExecEnv::from_process()
    };
    let runner =
        executors::build_runner_with(&cfg.repo.run, exec_env).map_err(|e| err("executor", e))?;

    let base = resolve_base(&repo, &cfg.repo.gate.base_ref).map_err(|e| err("base branch", e))?;
    let git = RealGit::new(
        repo.clone(),
        ctx.worktrees_dir.clone(),
        ctx.data_dir.join("locks"),
        base,
    );
    let clean = git.integrate(&[]).map_err(port)?;
    let hook_required = run_executor_is_claude(&cfg.repo.run);
    if hook_required {
        let problem = match git.base_sha() {
            Ok(base_sha) => {
                hook_settings::check_worktree(&clean.tree.path, &ctx.path_var, Some(&base_sha))
                    .err()
            }
            Err(e) => Some(format!("the base branch could not be read ({e})")),
        };
        if let Some(problem) = problem {
            let _ = git.discard(&clean);
            return Ok(CmdOutput::policy("refused", problem.clone())
                .field("task_id", json!(task.id))
                .field("reason", json!(problem)));
        }
    }

    let now = clock.now();
    let mut sandbox = sandbox_policy_for(
        sandboxed,
        &ctx.data_dir,
        &ctx.home,
        provider_for(&cfg.repo.run.executor),
        grant.account.as_ref(),
        &all_profiles,
    );
    sandbox.read_only = true;
    let base_request = AgentRequest {
        run_id: split_run_id(&task.id, now),
        prompt: String::new(),
        cwd: clean.tree.path.to_string_lossy().into_owned(),
        account: grant.account.clone(),
        timeout_secs: SPLIT_TIMEOUT_SECS,
        sandbox,
        json_schema: None,
    };
    let max_subtasks = usize::try_from(args.max)
        .unwrap_or(MAX_SPLIT_SUBTASKS)
        .clamp(2, MAX_SPLIT_SUBTASKS);

    let watch = SplitInfraWatch::new(&*runner);
    let split_res = propose_and_apply_split(
        &watch,
        &db,
        &task,
        &base_request,
        &cfg.repo.unblocker,
        max_subtasks,
        now,
        args.dry_run,
    );
    let _ = git.discard(&clean);
    let (_, limit_hit) = watch.take();
    if let (Some(account), Some(hit)) = (grant.account_id(), limit_hit.as_ref()) {
        let _ = pool.report_limit(account, hit);
    }
    let outcome = split_res.map_err(port)?;
    format_outcome(&outcome)
}

/// Proactively splits eligible `Ready` root tasks in `deps.settings.repo_root`
/// before `factory::run_with_audit` starts.
#[allow(dead_code)]
pub(crate) fn run_auto_split(deps: &Deps) -> Result<AutoSplitOutcome, CmdError> {
    run_auto_split_with_stop(deps, &|| false)
}

/// [`run_auto_split`] with a cooperative stop predicate checked between tasks.
pub(crate) fn run_auto_split_with_stop(
    deps: &Deps,
    should_stop: &dyn Fn() -> bool,
) -> Result<AutoSplitOutcome, CmdError> {
    let mut split_count = 0_u32;
    if should_stop() || is_frozen(&*deps.store).map_err(|e| err("store", e))? {
        return Ok(AutoSplitOutcome {
            split_count,
            halted: None,
        });
    }
    let ready = deps
        .store
        .list_tasks(Some(&deps.settings.repo_root), Some(TaskState::Ready))
        .map_err(|e| err("store", e))?;
    let profiles = deps.accounts.profiles().map_err(|e| err("accounts", e))?;

    for task in ready {
        if should_stop() || is_frozen(&*deps.store).map_err(|e| err("store", e))? {
            break;
        }
        if task.depth != 0 {
            continue;
        }
        if task.context_notes.iter().any(|n| n.starts_with("split:")) {
            continue;
        }
        if !deps
            .store
            .children_of(&task.id)
            .map_err(|e| err("store", e))?
            .is_empty()
        {
            continue;
        }
        let Some(grant) = deps
            .accounts
            .acquire_for(&task.id, Purpose::Work)
            .map_err(|e| err("accounts", e))?
        else {
            break;
        };
        let clean = deps.git.integrate(&[]).map_err(|e| err("auto-split", e))?;
        if deps.settings.hook_required {
            let exec = if run_executor_is_claude(&deps.settings.repo.run) {
                "claude"
            } else {
                deps.settings.repo.run.executor.as_str()
            };
            let problem = match deps.git.base_sha() {
                Ok(base_sha) => hook_settings::check_worktree_for_executor(
                    &clean.tree.path,
                    &deps.settings.agent_path,
                    Some(&base_sha),
                    exec,
                )
                .err(),
                Err(e) => Some(format!("the base branch could not be read ({e})")),
            };
            if let Some(problem) = problem {
                let _ = deps.git.discard(&clean);
                return Ok(AutoSplitOutcome {
                    split_count,
                    halted: Some(problem),
                });
            }
        }
        let now = deps.clock.now();
        let mut sandbox = sandbox_policy_for(
            deps.settings.sandboxed,
            &deps.settings.data_dir,
            &deps.settings.home,
            provider_for(&deps.settings.repo.run.executor),
            grant.account.as_ref(),
            &profiles,
        );
        sandbox.read_only = true;
        let base_request = AgentRequest {
            run_id: split_run_id(&task.id, now),
            prompt: String::new(),
            cwd: clean.tree.path.to_string_lossy().into_owned(),
            account: grant.account.clone(),
            timeout_secs: SPLIT_TIMEOUT_SECS,
            sandbox,
            json_schema: None,
        };
        let watch = SplitInfraWatch::new(&*deps.runner);
        let result = propose_and_apply_split(
            &watch,
            &*deps.store,
            &task,
            &base_request,
            &deps.settings.repo.unblocker,
            MAX_SPLIT_SUBTASKS,
            now,
            false,
        );
        let _ = deps.git.discard(&clean);
        let (infra, limit_hit) = watch.take();
        if let Some(hit) = limit_hit {
            if let Some(account) = grant.account_id() {
                deps.accounts
                    .report_limit(account, &hit)
                    .map_err(|e| err("accounts", e))?;
            }
            break;
        }
        if let Some(infra) = infra {
            let why = format!("{}: auto-split failed: {}", task.id, infra.raw);
            return Ok(AutoSplitOutcome {
                split_count,
                halted: Some(why),
            });
        }
        match result {
            Ok(outcome) => {
                if outcome.split {
                    split_count = split_count.saturating_add(1);
                }
            }
            Err(PortError::Io(msg)) if msg.starts_with("infra:") => {
                let why = format!("{}: auto-split failed: {msg}", task.id);
                return Ok(AutoSplitOutcome {
                    split_count,
                    halted: Some(why),
                });
            }
            Err(e) => return Err(err("auto-split", e)),
        }
    }

    Ok(AutoSplitOutcome {
        split_count,
        halted: None,
    })
}
