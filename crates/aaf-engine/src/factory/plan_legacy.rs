//! Legacy single-turn read-only plan stage (`pipeline.plan_stage = true` when
//! `pipeline.plan_debate = false`).

use aaf_domain::event::EventKind;
use aaf_domain::ports::{AgentOutcome, AgentRunner, PortResult};
use aaf_domain::task::{Task, TaskState};
use serde_json::json;

use super::Deps;
use super::adapters::provider_for;
use super::ports::{AccountPool, Grant, IntegrationGuard};
use super::prompt::{
    PLAN_APPROVED_PREFIX, PLAN_EXECUTOR, PromptContext, build_plan_prompt, plan_sandbox_policy_for,
};
use super::stage_router::StageTarget;
use super::unblock;
use super::worker::{self, Next};
use crate::git::TaskWorktree;
use crate::unblocker::FailureSignal;

const EMPTY_PLAN: &str = "the plan run ended without producing a plan";
const MAX_PLAN_CHARS: usize = 4000;
const PLAN_EVENTS_LIMIT: u32 = 100;

pub(super) fn needs_plan_stage(deps: &Deps, task: &Task) -> PortResult<bool> {
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

pub(super) fn run_plan_stage(
    deps: &Deps,
    task: &Task,
    grant: &Grant,
    stage: Option<&StageTarget>,
) -> PortResult<Next> {
    let accounts: &dyn AccountPool = stage.map_or(&*deps.accounts, |t| &*t.accounts);
    let clean = IntegrationGuard::acquire(&*deps.git, &[])?;
    let ran = run_plan_in_tree(deps, task, &clean.integration().tree, grant, stage);
    drop(clean);
    let Some((run_id, mut outcome)) = ran? else {
        return Ok(Next::Blocked);
    };
    if let Some(hit) = &outcome.limit_hit {
        unblock::report_limit_on(deps, &task.id, grant, accounts, hit)?;
        deps.move_to(&task.id, TaskState::Ready, None, "plan run hit usage limit")?;
        return unblock::route_limit_hit_on(grant, accounts, hit);
    }
    if let Some(infra) = &outcome.infra {
        return worker::halt_on_infra_with(deps, &task.id, grant, accounts, infra);
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
    unblock::handle_with(deps, &task.id, None, signal, grant, stage)
}

fn run_plan_in_tree(
    deps: &Deps,
    task: &Task,
    tree: &TaskWorktree,
    grant: &Grant,
    stage: Option<&StageTarget>,
) -> PortResult<Option<(String, AgentOutcome)>> {
    let (runner, accounts, provider, hook_req): (&dyn AgentRunner, &dyn AccountPool, _, bool) =
        match stage {
            Some(t) => (&*t.runner, &*t.accounts, t.provider_kind, t.hook_required),
            None => (
                &*deps.runner,
                &*deps.accounts,
                provider_for(&deps.settings.repo.run.executor),
                deps.settings.hook_required,
            ),
        };
    let exec = stage.map_or(deps.settings.repo.run.executor.as_str(), |t| {
        t.executor.as_str()
    });
    if let Some(problem) = worker::hook_problem_if(deps, &tree.path, hook_req, exec) {
        unblock::block_untried(deps, &task.id, &problem)?;
        return Ok(None);
    }
    let mut run = worker::begin_run_with(
        deps,
        task,
        tree,
        PLAN_EXECUTOR,
        provider,
        grant,
        hook_req,
        accounts,
    )?;
    let profiles = accounts.profiles()?;
    run.request.sandbox =
        plan_sandbox_policy_for(&deps.settings, provider, grant.account.as_ref(), &profiles);
    let profile = crate::config::effective_profile(&deps.settings.repo, &tree.path);
    let env_ctx = PromptContext {
        repo_root: &tree.path,
        profile: &profile,
        proof_required: deps.settings.repo.proof.required,
    };
    run.request.prompt = build_plan_prompt(task, &env_ctx);
    let outcome = worker::invoke_runner_with(deps, runner, task, &run)?;
    Ok(Some((run.request.run_id, outcome)))
}
