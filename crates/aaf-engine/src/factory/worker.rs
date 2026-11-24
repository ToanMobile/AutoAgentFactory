//! Jobs that need an account: the agent run of a task, the cross-audit of
//! its locked changes, and the unblocker ladder after a gate failure. Each
//! runs on its own thread and ends by saying where the task goes next.

use std::path::Path;

use aaf_domain::budget::{BudgetCheck, RootBudget};
use aaf_domain::event::EventKind;
use aaf_domain::gate::{GoldenScore, Receipt};
use aaf_domain::ports::{AgentOutcome, AgentRequest, PortError, PortResult};
use aaf_domain::run::{RunFinish, RunRecord, RunStatus};
use aaf_domain::task::{Task, TaskState};
use serde_json::json;

use super::audit_backend::{self, AuditBackend};
use super::audit_turn::{after_commit_failure, commit_earlier_attempt, note_no_change_reason};
pub(super) use super::audit_turn::{halt_message, halt_on_infra, infra_words};
use super::ports::{AccountPool, Grant};
use super::post_audit;
pub(crate) use super::prompt::PLAN_EXECUTOR;
use super::prompt::{
    PLAN_APPROVED_PREFIX, PromptContext, build_plan_prompt, build_prompt_with_context,
    plan_sandbox_policy, sandbox_policy,
};
use super::unblock::report_account_limit;
use super::{Deps, unblock};
use crate::cross_audit::AUDIT_EXECUTOR;
use crate::git::TaskWorktree;
use crate::hook_settings;
use crate::unblocker::{FailureSignal, PROPOSER_EXECUTOR};

const NO_CHANGE: &str = "the agent run ended without changing anything on the branch";
const EMPTY_PLAN: &str = "the plan run ended without producing a plan";
const MAX_PLAN_CHARS: usize = 4000;
const PLAN_EVENTS_LIMIT: u32 = 100;

/// Pre-gate head-lock audit outcome bound to `(tree_fp, base_sha)` for post-gate reuse.
#[derive(Debug, Clone)]
pub(crate) struct PreGateAudit {
    pub tree_fp: String,
    pub base_sha: Option<String>,
    pub audit_provider: String,
    pub reviewer: String,
    pub same_account: bool,
    pub scores: Vec<GoldenScore>,
}

/// A task waiting for the gate.
#[derive(Debug, Clone)]
pub(crate) struct GateJob {
    pub task_id: String,
    pub worktree: TaskWorktree,
    /// The cross-audit already ran for the current locked changes.
    pub audited: bool,
    /// Verified `Clear` receipt from before post-gate audit; re-verified before reuse.
    pub cached_receipt: Option<Box<Receipt>>,
}

pub(crate) enum Job {
    Agent {
        task_id: String,
    },
    Audit {
        job: GateJob,
        lock_fp: String,
    },
    PostAudit {
        job: GateJob,
        receipt: Box<Receipt>,
        pre_audit: Option<Box<PreGateAudit>>,
    },
    Unblock {
        job: GateJob,
        signal: Box<FailureSignal>,
    },
}

impl Job {
    /// A post-gate audit of `receipt` that audits afresh: no pre-gate
    /// verdict is reused (a retry, or a switch to another account).
    pub(crate) fn fresh_post_audit(job: GateJob, receipt: Receipt) -> Box<Job> {
        let receipt = Box::new(receipt);
        Box::new(Job::PostAudit {
            job,
            receipt,
            pre_audit: None,
        })
    }

    pub(crate) fn task_id(&self) -> &str {
        match self {
            Job::Agent { task_id } => task_id,
            Job::Audit { job, .. } | Job::PostAudit { job, .. } | Job::Unblock { job, .. } => {
                &job.task_id
            }
        }
    }
}

/// Where the task is after a job.
pub(crate) enum Next {
    Gate(GateJob),
    GateAudited(GateJob, Box<PreGateAudit>),
    Ready,
    /// Ready again because the account ran into its usage limit: the task
    /// itself has not been tried yet, and another account may take it.
    Switch,
    /// Like [`Next::Switch`], re-queueing an in-flight audit/post-audit job
    /// directly on another account without re-running the gate.
    SwitchJob(Box<Job>),
    /// The audit ended without a verdict: the same audit job runs once more.
    RetryAudit(Box<Job>),
    /// Ready again after a usage limit that no other account may take over
    /// (a pinned repository, or every account limited): the task waits for
    /// a cooldown and is not started again in this invocation.
    Wait {
        until: Option<i64>,
    },
    /// Ready again, untried: transient network failure. `Mode::Auto` retries
    /// with jittered backoff, then waits out an outage; `Mode::Once` halts.
    RetryAfter {
        until: i64,
        attempt: u32,
        detail: String,
    },
    /// Ready again, untried: the agent CLI failed for its account or the
    /// network. Every task started after it would fail the same way, so
    /// the run stops; the text says why and what to fix.
    Halt(String),
    /// Cross-audit rejected the changes; hand back to the scheduler so the
    /// unblocker runs on a real `Purpose::Work` grant from `deps.accounts`.
    Unblock {
        task_id: String,
        worktree: TaskWorktree,
        signal: Box<FailureSignal>,
    },
    Blocked,
}

pub(crate) struct Outcome {
    pub task_id: String,
    pub next: Next,
    pub error: Option<String>,
}

pub(crate) fn run_job(
    deps: &Deps,
    audit_backend: Option<&AuditBackend>,
    job: Job,
    grant: &Grant,
) -> Outcome {
    let task_id = job.task_id().to_string();
    let result = match job {
        Job::Agent { task_id } => agent(deps, &task_id, grant),
        Job::Audit { job, lock_fp } => {
            audit_backend::execute_audit(deps, audit_backend, job, &lock_fp, grant)
        }
        Job::PostAudit {
            job,
            receipt,
            pre_audit,
        } => {
            return post_audit::run_post_gate_audit(
                deps,
                audit_backend,
                job,
                *receipt,
                pre_audit,
                grant,
            );
        }
        Job::Unblock { job, signal } => {
            unblock::handle(deps, &job.task_id, Some(&job.worktree), *signal, grant)
        }
    };
    match result {
        Ok(next) => Outcome {
            task_id,
            next,
            error: None,
        },
        Err(err) => block_infra(deps, &task_id, &err.to_string()),
    }
}

/// An error of the factory's own machinery (git, store, executor) is not
/// something an agent retry fixes: the task waits for a human, without
/// freezing unrelated tasks across the factory.
pub(crate) fn block_infra(deps: &Deps, task_id: &str, error: &str) -> Outcome {
    let why = format!("factory error: {error}");
    let mut message = format!("{task_id}: {error}");
    if let Err(second) = unblock::block_untried(deps, task_id, &why) {
        // The store itself is failing; the summary is the only place left.
        message.push_str(&format!(" (and the task could not be blocked: {second})"));
    }
    Outcome {
        task_id: task_id.to_string(),
        next: Next::Blocked,
        error: Some(message),
    }
}

pub(crate) fn ensure_budget(deps: &Deps, root_id: &str) -> PortResult<RootBudget> {
    match deps.store.get_budget(root_id) {
        Err(PortError::NotFound(_)) => {
            let budget = RootBudget::new(root_id, &deps.settings.repo.unblocker);
            deps.store.put_budget(&budget)?;
            Ok(budget)
        }
        other => other,
    }
}

/// The request of a run on `grant`'s account in `cwd`; the prompt is filled
/// in by the caller.
pub(crate) fn request(deps: &Deps, cwd: &str, grant: &Grant) -> PortResult<AgentRequest> {
    request_with(
        deps,
        cwd,
        grant,
        deps.settings.hook_required,
        &*deps.accounts,
    )
}

fn request_with(
    deps: &Deps,
    cwd: &str,
    grant: &Grant,
    hook_required: bool,
    accounts: &dyn AccountPool,
) -> PortResult<AgentRequest> {
    let settings = &deps.settings;
    // Every agent the factory starts (task run, cross-audit, subtask
    // proposal) gets its request here: none starts where the hook is not
    // registered (for executors that require `aaf-hook`).
    if let Some(problem) = hook_problem_if(deps, Path::new(cwd), hook_required) {
        return Err(PortError::Invalid(problem));
    }
    Ok(AgentRequest {
        run_id: unblock::unique_id(),
        prompt: String::new(),
        cwd: cwd.to_string(),
        account: grant.account.clone(),
        timeout_secs: settings.repo.run.run_timeout_secs,
        sandbox: sandbox_policy(
            settings.sandboxed,
            &settings.data_dir,
            &settings.home,
            grant.account.as_ref(),
            &accounts.profiles()?,
        ),
    })
}

/// Why an agent started in `cwd` would run without `aaf-hook`; `None` when
/// it is registered there or the executor has no such hook.
pub(crate) fn hook_problem(deps: &Deps, cwd: &Path) -> Option<String> {
    hook_problem_if(deps, cwd, deps.settings.hook_required)
}

fn hook_problem_if(deps: &Deps, cwd: &Path, required: bool) -> Option<String> {
    let settings = &deps.settings;
    if !required {
        return None;
    }
    // The main checkout is the owner's and no agent writes there; every
    // other directory is a worktree cut from the base, and its settings
    // file must still be the base's.
    let base = if cwd == Path::new(&settings.repo_root) {
        None
    } else {
        match deps.git.base_sha() {
            Ok(base) => Some(base),
            Err(err) => return Some(format!("the base branch could not be read ({err})")),
        }
    };
    hook_settings::check_worktree(cwd, &settings.agent_path, base.as_deref()).err()
}

pub(super) struct OpenRun {
    pub(super) request: AgentRequest,
    started: i64,
}

pub(super) fn begin_run_with(
    deps: &Deps,
    task: &Task,
    worktree: &TaskWorktree,
    executor: &str,
    grant: &Grant,
    hook_required: bool,
    accounts: &dyn AccountPool,
) -> PortResult<OpenRun> {
    let request = request_with(
        deps,
        &worktree.path.to_string_lossy(),
        grant,
        hook_required,
        accounts,
    )?;
    let started = deps.now();
    deps.store.insert_run(&RunRecord {
        id: request.run_id.clone(),
        task_id: task.id.clone(),
        account_id: grant.account_id().map(str::to_string),
        executor: executor.to_string(),
        worktree_path: request.cwd.clone(),
        branch: worktree.branch.clone(),
        status: RunStatus::Running,
        exit_code: None,
        tokens_in: 0,
        tokens_out: 0,
        started_at: started,
        ended_at: None,
    })?;
    deps.emit(
        EventKind::RunStarted,
        &task.id,
        json!({ "run_id": request.run_id, "executor": executor, "account": grant.account_id() }),
    )?;
    Ok(OpenRun { request, started })
}

/// Closes the run and charges the root budget: tokens are input + output +
/// cache creation (`tokens_in` already includes cache creation).
pub(super) fn end_run(
    deps: &Deps,
    task: &Task,
    run: &OpenRun,
    status: RunStatus,
    exit_code: Option<i32>,
    tokens: (u64, u64),
    tokens_measured: bool,
) -> PortResult<()> {
    let ended = deps.now();
    deps.store.finish_run(
        &run.request.run_id,
        &RunFinish {
            status,
            exit_code,
            tokens_in: tokens.0,
            tokens_out: tokens.1,
            ended_at: ended,
        },
    )?;
    let wall = u64::try_from(ended - run.started).unwrap_or(0);
    ensure_budget(deps, &task.root_id)?;
    deps.store
        .add_usage(&task.root_id, tokens.0.saturating_add(tokens.1), wall, 0)?;
    deps.emit(
        EventKind::RunFinished,
        &task.id,
        json!({
            "run_id": run.request.run_id,
            "status": status.as_str(),
            "tokens_in": tokens.0,
            "tokens_out": tokens.1,
            "tokens_measured": tokens_measured,
        }),
    )
}

pub(super) fn status_of(outcome: &AgentOutcome) -> RunStatus {
    if outcome.limit_hit.is_some() {
        RunStatus::LimitHit
    } else if outcome.timed_out {
        RunStatus::TimedOut
    } else if outcome.exit_code == Some(0) {
        RunStatus::Succeeded
    } else {
        RunStatus::Failed
    }
}

fn agent(deps: &Deps, task_id: &str, grant: &Grant) -> PortResult<Next> {
    let task = deps.store.get_task(task_id)?;
    let budget = ensure_budget(deps, &task.root_id)?.check();
    if budget != BudgetCheck::Ok {
        // No run on an exhausted budget; the ladder freezes the root.
        let mut signal = FailureSignal::empty();
        signal.budget = budget;
        return unblock::handle(deps, task_id, None, signal, grant);
    }
    if needs_plan(deps, &task)? {
        return run_plan_stage(deps, &task, grant);
    }
    let worktree = deps.git.task_worktree(task_id)?;
    let has_impl_runs = deps.store.runs_for_task(task_id)?.iter().any(|run| {
        run.executor != PLAN_EXECUTOR
            && run.executor != AUDIT_EXECUTOR
            && run.executor != PROPOSER_EXECUTOR
    });
    if has_impl_runs {
        // A failed or interrupted run leaves its edits uncommitted, and git
        // refuses to rebase a dirty tree: keep them as a commit, the next
        // attempt builds on them.
        if let Some(next) = commit_earlier_attempt(deps, &task, &worktree)? {
            return Ok(next);
        }
    } else if hook_problem(deps, &worktree.path).is_some() {
        // The worktree may have been cut before the registration was
        // committed on the base: nothing ran in it yet, so it can follow.
        deps.git.rebase(&worktree)?;
    }
    if let Some(problem) = hook_problem(deps, &worktree.path) {
        // Not a failure a retry fixes, and no run is recorded: nothing ran.
        // Nor a failed root: the task was never tried.
        unblock::block_untried(deps, task_id, &problem)?;
        return Ok(Next::Blocked);
    }

    let mut run = begin_run_with(
        deps,
        &task,
        &worktree,
        &deps.settings.repo.run.executor,
        grant,
        deps.settings.hook_required,
        &*deps.accounts,
    )?;
    if let Err(err) = snapshot_turn(deps, &task.id, &worktree, &run.request.run_id) {
        let _ = end_run(deps, &task, &run, RunStatus::Failed, None, (0, 0), false);
        return Err(err);
    }
    let profile = crate::config::effective_profile(&deps.settings.repo, &worktree.path);
    let env_ctx = PromptContext {
        repo_root: &worktree.path,
        profile: &profile,
        proof_required: deps.settings.repo.proof.required,
    };
    run.request.prompt = build_prompt_with_context(&task, &worktree.branch, &env_ctx);
    let mut outcome = invoke_runner(deps, &task, &run)?;
    if let Some(hit) = &outcome.limit_hit {
        report_account_limit(deps, task_id, grant, hit)?;
    }
    if let Some(infra) = &outcome.infra {
        return halt_on_infra(deps, task_id, grant, infra);
    }

    if outcome.succeeded() {
        if let Err(error) = deps
            .git
            .commit_all(&worktree, &format!("aaf: {}", task.title))
        {
            return after_commit_failure(deps, task_id, &worktree, outcome, error, grant);
        }
        if let Some(problem) = hook_problem(deps, &worktree.path) {
            // The run changed its own guard. Nothing of it goes on to the
            // gate, a pull request or the base, where the changed file
            // would be loaded by the owner's own sessions.
            unblock::block_untried(deps, task_id, &problem)?;
            return Ok(Next::Blocked);
        }
        if deps.git.has_changes(&worktree)? {
            deps.move_to(task_id, TaskState::Gating, None, "agent run finished")?;
            return Ok(Next::Gate(GateJob {
                task_id: task_id.to_string(),
                worktree,
                audited: false,
                cached_receipt: None,
            }));
        }
        // A fixed text, so repeated empty runs share one failure digest.
        outcome.log_tail = NO_CHANGE.to_string();
        deps.store
            .append_context_note(task_id, NO_CHANGE, deps.now())?;
        note_no_change_reason(deps, task_id, &outcome.final_text)?;
    }
    let mut signal = FailureSignal::empty();
    signal.outcome = Some(outcome);
    unblock::handle(deps, task_id, Some(&worktree), signal, grant)
}

fn snapshot_turn(deps: &Deps, task_id: &str, wt: &TaskWorktree, run_id: &str) -> PortResult<()> {
    let now = deps.now();
    let created = deps.git.create_turn_snapshot(wt, run_id, now);
    if let Some((r, oid)) = created.map_err(|e| PortError::Io(e.to_string()))? {
        deps.store
            .record_turn_snapshot(task_id, run_id, &r, &oid, now)?;
    }
    Ok(())
}

fn invoke_runner(deps: &Deps, task: &Task, run: &OpenRun) -> PortResult<AgentOutcome> {
    let outcome = match deps.runner.run(&run.request) {
        Ok(outcome) => outcome,
        Err(err) => {
            end_run(deps, task, run, RunStatus::Failed, None, (0, 0), false)?;
            return Err(err);
        }
    };
    let tokens = (outcome.tokens_in, outcome.tokens_out);
    let status = status_of(&outcome);
    end_run(
        deps,
        task,
        run,
        status,
        outcome.exit_code,
        tokens,
        deps.runner.reports_tokens(),
    )?;
    Ok(outcome)
}

fn needs_plan(deps: &Deps, task: &Task) -> PortResult<bool> {
    if !deps.settings.repo.pipeline.plan_stage
        || task
            .context_notes
            .iter()
            .any(|n| n.starts_with(PLAN_APPROVED_PREFIX))
    {
        return Ok(false);
    }
    let events = deps.store.events_for_task(&task.id, PLAN_EVENTS_LIMIT)?;
    Ok(!events.iter().any(|e| e.kind == EventKind::PlanGenerated))
}

fn run_plan_stage(deps: &Deps, task: &Task, grant: &Grant) -> PortResult<Next> {
    let clean = deps.git.integrate(&[])?;
    let ran = run_plan_in_tree(deps, task, &clean.tree, grant);
    let _ = deps.git.discard(&clean);
    let Some((run_id, mut outcome)) = ran? else {
        return Ok(Next::Blocked);
    };
    if let Some(hit) = &outcome.limit_hit {
        report_account_limit(deps, &task.id, grant, hit)?;
        deps.move_to(&task.id, TaskState::Ready, None, "plan run hit usage limit")?;
        return unblock::route_limit_hit(deps, grant, hit);
    }
    if let Some(infra) = &outcome.infra {
        return halt_on_infra(deps, &task.id, grant, infra);
    }
    let plan = outcome.final_text.trim();
    if outcome.succeeded() && !plan.is_empty() {
        let truncated: String = plan.chars().take(MAX_PLAN_CHARS).collect();
        let payload = json!({ "plan": truncated, "run_id": run_id });
        deps.emit(EventKind::PlanGenerated, &task.id, payload)?;
        if deps.settings.repo.pipeline.require_plan_approval {
            let summary = unblock::one_line(&truncated, 200);
            let why = format!("plan approval required: {summary}");
            unblock::block_untried(deps, &task.id, &why)?;
            return Ok(Next::Blocked);
        }
        let note = format!("{PLAN_APPROVED_PREFIX} {truncated}");
        deps.store
            .append_context_note(&task.id, &note, deps.now())?;
        deps.move_to(&task.id, TaskState::Ready, None, "plan generated")?;
        return Ok(Next::Ready);
    }
    if outcome.succeeded() {
        outcome.log_tail = EMPTY_PLAN.to_string();
    }
    let mut signal = FailureSignal::empty();
    signal.outcome = Some(outcome);
    unblock::handle(deps, &task.id, None, signal, grant)
}

fn run_plan_in_tree(
    deps: &Deps,
    task: &Task,
    tree: &TaskWorktree,
    grant: &Grant,
) -> PortResult<Option<(String, AgentOutcome)>> {
    if let Some(problem) = hook_problem(deps, &tree.path) {
        unblock::block_untried(deps, &task.id, &problem)?;
        return Ok(None);
    }
    let mut run = begin_run_with(
        deps,
        task,
        tree,
        PLAN_EXECUTOR,
        grant,
        deps.settings.hook_required,
        &*deps.accounts,
    )?;
    let profiles = deps.accounts.profiles()?;
    run.request.sandbox = plan_sandbox_policy(&deps.settings, grant.account.as_ref(), &profiles);
    let profile = crate::config::effective_profile(&deps.settings.repo, &tree.path);
    let env_ctx = PromptContext {
        repo_root: &tree.path,
        profile: &profile,
        proof_required: deps.settings.repo.proof.required,
    };
    run.request.prompt = build_plan_prompt(task, &env_ctx);
    let outcome = invoke_runner(deps, task, &run)?;
    Ok(Some((run.request.run_id, outcome)))
}
