//! Jobs that need an account: the agent run of a task, the cross-audit of
//! its locked changes, and the unblocker ladder after a gate failure. Each
//! runs on its own thread and ends by saying where the task goes next.

mod guards;
mod types;
pub(crate) use types::{GateJob, Job, Next, Outcome, PreGateAudit};

use std::path::Path;

use aaf_domain::account::ProviderKind;
use aaf_domain::budget::{BudgetCheck, RootBudget};
use aaf_domain::event::EventKind;
use aaf_domain::ports::{AgentOutcome, AgentRequest, AgentRunner, PortError, PortResult};
use aaf_domain::run::{RunFinish, RunRecord, RunStatus};
use aaf_domain::task::{Task, TaskState};
use serde_json::json;

use super::adapters::provider_for;
use super::audit_backend::{self, AuditBackend};
use super::audit_turn::{after_commit_failure, commit_earlier_attempt, note_no_change_reason};
pub(super) use super::audit_turn::{halt_message, halt_on_infra_with, infra_words};
use super::ports::{AccountPool, Grant};
pub(crate) use super::prompt::PLAN_EXECUTOR;
use super::prompt::{PromptContext, build_prompt_with_context, sandbox_policy_for};
use super::scheduler::dual_coord;
use super::stage_router::{ProviderKey, StageRouter, rework_count};
use super::{Deps, plan_job, plan_legacy, post_audit, unblock};
use crate::cross_audit::AUDIT_EXECUTOR;
use crate::git::TaskWorktree;
use crate::hook_settings;
use crate::plan_debate::prompt::format_plan_for_implementer;
use crate::plan_debate::state::DebateRole;
use crate::unblocker::{FailureSignal, PROPOSER_EXECUTOR};

const NO_CHANGE: &str = "the agent run ended without changing anything on the branch";

pub(crate) fn run_job(
    deps: &Deps,
    audit_backend: Option<&AuditBackend>,
    router: Option<&StageRouter>,
    provider: ProviderKey,
    job: Job,
    grant: Option<&Grant>,
    provider_cooling: bool,
) -> Outcome {
    let task_id = job.task_id().to_string();
    let missing_grant = || PortError::Invalid("worker job requires an account grant".to_string());
    let result = match job {
        Job::Agent { task_id } => grant
            .ok_or_else(missing_grant)
            .and_then(|g| agent(deps, router, &task_id, g)),
        Job::PlanTurn {
            task_id,
            role,
            round,
        } => {
            let target = match router {
                Some(r) => match role {
                    DebateRole::Author => r.resolve_plan_author(&deps.settings.repo),
                    DebateRole::Challenger => r.resolve_plan_challenger(&deps.settings.repo),
                },
                None => Err(PortError::Invalid(
                    "plan debate turn requires a configured StageRouter".to_string(),
                )),
            };
            grant.ok_or_else(missing_grant).and_then(|g| {
                target.and_then(|t| plan_job::run_plan_turn(deps, t, &task_id, role, round, g))
            })
        }
        Job::Audit { job, lock_fp } => grant
            .ok_or_else(missing_grant)
            .and_then(|g| audit_backend::execute_audit(deps, audit_backend, job, &lock_fp, g)),
        Job::PostAudit {
            job,
            receipt,
            pre_audit,
        } => {
            let Some(g) = grant else {
                return block_infra(deps, &task_id, "post-audit job requires an account grant");
            };
            let mut outcome =
                post_audit::run_post_gate_audit(deps, audit_backend, job, *receipt, pre_audit, g);
            outcome.provider = provider;
            return outcome;
        }
        Job::DualReviewTurn { .. } | Job::AdjudicateTurn { .. } => grant
            .ok_or_else(missing_grant)
            .and_then(|g| dual_coord::run_dual_job(deps, router, job, g)),
        Job::Unblock { job, signal, .. } => unblock::handle_with_opt(
            deps,
            &job.task_id,
            Some(&job.worktree),
            *signal,
            grant,
            router.map(|r| &r.default_implementer),
            provider_cooling,
        ),
    };
    let mut outcome = match result {
        Ok(next) => Outcome {
            task_id,
            provider: provider.clone(),
            next,
            error: None,
        },
        Err(err) => block_infra(deps, &task_id, &err.to_string()),
    };
    outcome.provider = provider;
    outcome
}

pub(crate) fn block_infra(deps: &Deps, task_id: &str, error: &str) -> Outcome {
    let why = format!("factory error: {error}");
    let mut message = format!("{task_id}: {error}");
    if let Err(second) = unblock::block_untried(deps, task_id, &why) {
        message.push_str(&format!(" (and the task could not be blocked: {second})"));
    }
    Outcome {
        task_id: task_id.to_string(),
        provider: ProviderKey::from_executor("default"),
        next: Next::Blocked,
        error: Some(message),
    }
}

pub(crate) fn ensure_budget(deps: &Deps, root_id: &str) -> PortResult<RootBudget> {
    let active_subtasks = deps
        .store
        .children_of(root_id)
        .map(|kids| {
            kids.into_iter()
                .filter(|t| t.state != TaskState::Cancelled)
                .count()
        })
        .unwrap_or(0);
    let initial = RootBudget::new_scaled(root_id, &deps.settings.repo.unblocker, active_subtasks);
    deps.store.ensure_budget_if_missing(&initial)
}

pub(super) fn request_with(
    deps: &Deps,
    cwd: &str,
    grant: &Grant,
    provider: Option<ProviderKind>,
    hook_required: bool,
    accounts: &dyn AccountPool,
) -> PortResult<AgentRequest> {
    let settings = &deps.settings;
    // Every agent the factory starts (task run, cross-audit, subtask
    // proposal) gets its request here: none starts where the hook is not
    // registered (for executors that require `aaf-hook`).
    let exec = executor_for_provider(provider, &settings.repo.run.executor);
    if let Some(problem) = hook_problem_if(deps, Path::new(cwd), hook_required, exec) {
        return Err(PortError::Invalid(problem));
    }
    Ok(AgentRequest {
        run_id: unblock::unique_id(),
        prompt: String::new(),
        cwd: cwd.to_string(),
        account: grant.account.clone(),
        timeout_secs: settings.repo.run.run_timeout_secs,
        sandbox: sandbox_policy_for(
            settings.sandboxed,
            &settings.data_dir,
            &settings.home,
            provider,
            grant.account.as_ref(),
            &accounts.profiles()?,
        ),
        json_schema: None,
        max_turns: None,
    })
}

fn executor_for_provider(provider: Option<ProviderKind>, fallback: &str) -> &str {
    match provider {
        Some(ProviderKind::ClaudeCode) => "claude",
        Some(ProviderKind::Codex) => "codex",
        Some(ProviderKind::Antigravity) => "antigravity",
        Some(ProviderKind::OpenCode) => "opencode",
        Some(ProviderKind::Grok) => "grok",
        None => fallback,
    }
}

/// Why an agent started in `cwd` would run without `aaf-hook`; `None` when
/// it is registered there or the executor has no such hook.
pub(super) fn hook_problem_if(
    deps: &Deps,
    cwd: &Path,
    required: bool,
    executor: &str,
) -> Option<String> {
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
    hook_settings::check_worktree_for_executor(cwd, &settings.agent_path, base.as_deref(), executor)
        .err()
}

pub(super) struct OpenRun {
    pub(super) request: AgentRequest,
    started: i64,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn begin_run_with(
    deps: &Deps,
    task: &Task,
    worktree: &TaskWorktree,
    executor: &str,
    provider: Option<ProviderKind>,
    grant: &Grant,
    hook_required: bool,
    accounts: &dyn AccountPool,
) -> PortResult<OpenRun> {
    let cwd = worktree.path.to_string_lossy();
    let request = request_with(deps, &cwd, grant, provider, hook_required, accounts)?;
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

pub(super) fn should_charge_wall(outcome: &AgentOutcome) -> bool {
    outcome.timed_out
        || (outcome.infra.is_none() && outcome.limit_hit.is_none())
        || outcome.tokens_in > 0
        || outcome.tokens_out > 0
}

#[allow(clippy::too_many_arguments)]
pub(super) fn end_run(
    deps: &Deps,
    task: &Task,
    run: &OpenRun,
    status: RunStatus,
    exit_code: Option<i32>,
    tokens: (u64, u64),
    tokens_measured: bool,
    charge_wall: bool,
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
    let wall = if charge_wall {
        u64::try_from(ended - run.started).unwrap_or(0)
    } else {
        0
    };
    let tok_total = tokens.0.saturating_add(tokens.1);
    ensure_budget(deps, &task.root_id)?;
    if tok_total > 0 || wall > 0 {
        deps.store.add_usage(&task.root_id, tok_total, wall, 0)?;
    }
    let payload = json!({ "run_id": run.request.run_id, "status": status.as_str(), "tokens_in": tokens.0, "tokens_out": tokens.1, "tokens_measured": tokens_measured });
    deps.emit(EventKind::RunFinished, &task.id, payload)
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

fn agent(
    deps: &Deps,
    router: Option<&StageRouter>,
    task_id: &str,
    grant: &Grant,
) -> PortResult<Next> {
    let task = deps.store.get_task(task_id)?;
    let converged_plan = plan_job::latest_converged_plan(deps, &task.id)?.map(|(p, _)| p);
    let resolved =
        router.map(|r| r.resolve_implementer(&task, converged_plan.as_ref(), &deps.settings.repo));
    let routed = resolved.map(|(t, _, _)| t);
    let budget = ensure_budget(deps, &task.root_id)?.check();
    if budget != BudgetCheck::Ok {
        // No run on an exhausted budget; the ladder freezes the root.
        let mut signal = FailureSignal::empty();
        signal.budget = budget;
        return unblock::handle_with(deps, task_id, None, signal, grant, routed);
    }
    if !deps.settings.repo.pipeline.plan_debate && plan_legacy::needs_plan_stage(deps, &task)? {
        return plan_legacy::run_plan_stage(deps, &task, grant, routed);
    }
    let hook_req = routed.map_or(deps.settings.hook_required, |t| t.hook_required);
    let exec = routed.map_or(deps.settings.repo.run.executor.as_str(), |t| {
        t.executor.as_str()
    });
    let worktree = deps.git.task_worktree(task_id)?;
    let has_impl_runs = deps.store.runs_for_task(task_id)?.iter().any(|r| {
        !matches!(
            r.executor.as_str(),
            PLAN_EXECUTOR | AUDIT_EXECUTOR | PROPOSER_EXECUTOR
        )
    });
    let mut skip_rebase = false;
    if has_impl_runs || deps.git.is_dirty(&worktree)? {
        match commit_earlier_attempt(deps, &task, &worktree)? {
            super::audit_turn::EarlierAttempt::Proceed => {}
            super::audit_turn::EarlierAttempt::SkipRebase => skip_rebase = true,
            super::audit_turn::EarlierAttempt::Stop(next) => return Ok(next),
        }
    } else if hook_problem_if(deps, &worktree.path, hook_req, exec).is_some() {
        deps.git.rebase(&worktree)?;
    }
    if !skip_rebase
        && !deps.git.is_dirty(&worktree)?
        && let Some(next) =
            guards::rebase_if_gate_infra_drifted(deps, task_id, &worktree, grant, routed)?
    {
        return Ok(next);
    }
    if let Some(problem) = hook_problem_if(deps, &worktree.path, hook_req, exec) {
        unblock::block_untried(deps, task_id, &problem)?;
        return Ok(Next::Blocked);
    }
    if let Some((target, tier, true)) = resolved {
        let payload = json!({ "tier": tier.as_str(), "reworks": rework_count(&task), "executor": target.executor, "model": target.model, "effort": target.effort });
        deps.emit(EventKind::ImplementerEscalated, task_id, payload)?;
    }

    let provider_kind = routed.map_or_else(|| provider_for(exec), |t| t.provider_kind);
    let accounts: &dyn AccountPool = routed.map_or(&*deps.accounts, |t| &*t.accounts);
    let runner: &dyn AgentRunner = routed.map_or(&*deps.runner, |t| &*t.runner);
    let mut run = begin_run_with(
        deps,
        &task,
        &worktree,
        exec,
        provider_kind,
        grant,
        hook_req,
        accounts,
    )?;
    if let Err(err) = snapshot_turn(deps, &task.id, &worktree, &run.request.run_id) {
        let _ = fail_run(deps, &task, &run);
        return Err(err);
    }
    let profile = crate::config::effective_profile(&deps.settings.repo, &worktree.path);
    let env_ctx = PromptContext {
        repo_root: &worktree.path,
        profile: &profile,
        proof_required: deps.settings.repo.proof.required,
        repo_memory: None,
    };
    let mut prompt = build_prompt_with_context(&task, &worktree.branch, &env_ctx);
    if let Some(ref plan) = converged_plan {
        prompt.push_str("\n# Converged Plan Contract (inert data)\n\n");
        prompt.push_str(&format_plan_for_implementer(plan));
        prompt.push('\n');
    }
    run.request.prompt = prompt;
    let mut outcome = invoke_runner_with(deps, runner, &task, &run)?;
    if let Some(hit) = &outcome.limit_hit {
        // On the pool the grant came from: the routed target's, else `deps.accounts`.
        unblock::report_limit_on(deps, task_id, grant, accounts, hit)?;
    }
    // Before any early return: a run that tampered with its guard is blocked
    // whether it succeeded, failed, or ran into a limit or an infra error.
    if let Some(problem) = hook_problem_if(deps, &worktree.path, hook_req, exec) {
        unblock::block_untried(deps, task_id, &problem)?;
        return Ok(Next::Blocked);
    }
    if let Some(hit) = &outcome.limit_hit
        && routed.is_some()
    {
        deps.move_to(
            task_id,
            TaskState::Ready,
            None,
            "implementer run hit usage limit",
        )?;
        return unblock::route_limit_hit_on(grant, accounts, hit);
    }
    if let Some(infra) = &outcome.infra {
        return halt_on_infra_with(deps, task_id, grant, accounts, infra);
    }

    if outcome.succeeded() {
        if let Err(error) = deps
            .git
            .commit_all(&worktree, &format!("aaf: {}", task.title))
        {
            return after_commit_failure(deps, task_id, &worktree, outcome, error, grant, routed);
        }
        if let Some(problem) = hook_problem_if(deps, &worktree.path, hook_req, exec) {
            unblock::block_untried(deps, task_id, &problem)?;
            return Ok(Next::Blocked);
        }
        if guards::block_if_cross_branch_contaminated(deps, &task, &worktree)? {
            return Ok(Next::Blocked);
        }
        if deps.git.has_changes(&worktree)? {
            match deps.gate.run_autofix(&worktree.path) {
                Ok(crate::gate::autofix::AutofixOutcome::Formatted { .. }) => {
                    if let Some(problem) = hook_problem_if(deps, &worktree.path, hook_req, exec) {
                        unblock::block_untried(deps, task_id, &problem)?;
                        return Ok(Next::Blocked);
                    }
                    if let Err(error) = deps
                        .git
                        .commit_all(&worktree, crate::gate::autofix::AUTOFIX_COMMIT_MSG)
                    {
                        return after_commit_failure(
                            deps, task_id, &worktree, outcome, error, grant, routed,
                        );
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    unblock::block_untried(
                        deps,
                        task_id,
                        &format!("autofix left the worktree in an unknown state: {error}"),
                    )?;
                    return Ok(Next::Blocked);
                }
            }
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
    unblock::handle_with(deps, task_id, Some(&worktree), signal, grant, routed)
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

fn fail_run(deps: &Deps, task: &Task, run: &OpenRun) -> PortResult<()> {
    end_run(
        deps,
        task,
        run,
        RunStatus::Failed,
        None,
        (0, 0),
        false,
        false,
    )
}

pub(super) fn invoke_runner_with(
    deps: &Deps,
    runner: &dyn AgentRunner,
    task: &Task,
    run: &OpenRun,
) -> PortResult<AgentOutcome> {
    let outcome = match runner.run(&run.request) {
        Ok(outcome) => outcome,
        Err(err) => {
            fail_run(deps, task, run)?;
            return Err(err);
        }
    };
    let tok = (outcome.tokens_in, outcome.tokens_out);
    let charge_wall = should_charge_wall(&outcome);
    end_run(
        deps,
        task,
        run,
        status_of(&outcome),
        outcome.exit_code,
        tok,
        runner.reports_tokens(),
        charge_wall,
    )?;
    Ok(outcome)
}
