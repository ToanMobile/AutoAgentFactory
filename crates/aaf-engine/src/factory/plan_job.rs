//! Plan Debate turn execution (`Job::PlanTurn`) for Adversarial Dual-Frontier
//! planning (`docs/PLAN_DUAL_FRONTIER.md` §D.2, R3-CONC-2, R3-CONC-4).

use std::path::Path;

use aaf_domain::budget::BudgetCheck;
use aaf_domain::event::EventKind;
use aaf_domain::pipeline::{PlanContract, PlanCritique};
use aaf_domain::ports::{AgentOutcome, InfraFailure, PortResult};
use aaf_domain::run::RunStatus;
use aaf_domain::task::{Task, TaskState};
use serde_json::json;

use super::audit_backend;
use super::ports::Grant;
use super::prompt::{PLAN_EXECUTOR, PromptContext, STEER_PREFIX};
use super::stage_router::{StageTarget, is_tier_s_skip_debate};
use super::worker::{self, Next};
use super::{Deps, unblock};
use crate::git::TaskWorktree;
use crate::plan_debate::prompt::{
    build_author_prompt, build_challenger_prompt, build_plan_format_repair_prompt,
};
use crate::plan_debate::schema::{
    canonical_plan_hash, format_plan_approved_note, plan_contract_json_schema,
    plan_critique_json_schema, validate_and_normalize_plan, validate_plan_critique,
};
use crate::plan_debate::state::{
    DebateNext, DebateRole, DebateSnapshot, evaluate_critique, next_step, replay_debate_state,
};
use crate::plan_debate::{MAX_CONSECUTIVE_TURN_FAILS, PLAN_DEBATE_STALLED_PREFIX};

/// Prefix used when a converged plan awaits explicit human approval.
pub const PLAN_APPROVAL_PREFIX: &str = "plan approval required:";
const DEBATE_KINDS: &[EventKind] = &[
    EventKind::PlanDebateTurn,
    EventKind::PlanConverged,
    EventKind::TaskStateChanged,
    EventKind::ReworkRequested,
];

struct TurnCtx<'a> {
    deps: &'a Deps,
    target: &'a StageTarget,
    task: &'a Task,
    tree: &'a TaskWorktree,
    grant: &'a Grant,
    round: u32,
    role: &'static str,
    prior_fails: u32,
    fallback_plan: Option<(&'a PlanContract, &'a str)>,
}

enum Parsed<T> {
    Ok(String, T),
    Exit(Next),
}

fn load_snapshot(deps: &Deps, task_id: &str) -> PortResult<DebateSnapshot> {
    let events = deps
        .store
        .events_for_task_by_kinds(task_id, DEBATE_KINDS, u32::MAX)?;
    Ok(replay_debate_state(&events))
}

/// Returns the latest converged `(PlanContract, hash)` in the active epoch.
pub(crate) fn latest_converged_plan(
    deps: &Deps,
    task_id: &str,
) -> PortResult<Option<(PlanContract, String)>> {
    Ok(load_snapshot(deps, task_id)?.converged)
}

/// Computes the next Plan Debate step for a `Ready` task, or `Ok(None)` when
/// debate is disabled or Tier-S skipped.
pub(crate) fn next_debate_step_for_task(
    deps: &Deps,
    task: &Task,
) -> PortResult<Option<DebateNext>> {
    if !deps.settings.repo.pipeline.plan_debate || is_tier_s_skip_debate(task, &deps.settings.repo)
    {
        return Ok(None);
    }
    let snap = load_snapshot(deps, &task.id)?;
    if let Some((plan, plan_hash)) = snap.converged {
        return Ok(Some(DebateNext::Converged {
            plan: Box::new(plan),
            plan_hash,
        }));
    }
    Ok(Some(next_step(
        &snap,
        deps.settings.repo.pipeline.max_plan_rounds,
    )))
}

/// Runs one Plan Debate turn on an isolated clean base worktree in read-only mode.
/// Upholds R3-CONC-2 (`Ready -> Running -> Ready`) and R3-CONC-4 (uses only
/// `target.accounts` and `block_untried`, never `deps.accounts` or `unblock::handle`).
pub(crate) fn run_plan_turn(
    deps: &Deps,
    target: &StageTarget,
    task_id: &str,
    role: DebateRole,
    round: u32,
    grant: &Grant,
) -> PortResult<Next> {
    let clean = deps.git.integrate(&[])?;
    let res = run_plan_turn_in(deps, target, task_id, role, round, grant, &clean.tree);
    let _ = deps.git.discard(&clean);
    res
}

fn run_plan_turn_in(
    deps: &Deps,
    target: &StageTarget,
    task_id: &str,
    role: DebateRole,
    round: u32,
    grant: &Grant,
    tree: &TaskWorktree,
) -> PortResult<Next> {
    let task = deps.store.get_task(task_id)?;
    let snap = load_snapshot(deps, task_id)?;
    let root = &tree.path;
    if let Some(problem) = worker::hook_problem_if(deps, root, target.hook_required) {
        unblock::block_untried(deps, task_id, &problem)?;
        return Ok(Next::Blocked);
    }
    let profile = crate::config::effective_profile(&deps.settings.repo, root);
    let repo_ctx = PromptContext {
        repo_root: root,
        profile: &profile,
        proof_required: deps.settings.repo.proof.required,
    };
    let steering: Vec<String> = task
        .context_notes
        .iter()
        .filter_map(|n| n.strip_prefix(STEER_PREFIX).map(str::trim))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let prev_pair = snap.latest_plan.as_ref().map(|p| {
        let h = snap
            .latest_plan_hash
            .clone()
            .unwrap_or_else(|| canonical_plan_hash(p));
        (p, h)
    });
    let ctx = TurnCtx {
        deps,
        target,
        task: &task,
        tree,
        grant,
        round,
        role: match role {
            DebateRole::Author => "author",
            DebateRole::Challenger => "challenger",
        },
        prior_fails: snap.consecutive_fails,
        fallback_plan: prev_pair.as_ref().map(|(p, h)| (*p, h.as_str())),
    };
    match role {
        DebateRole::Author => {
            let prompt = build_author_prompt(
                &task,
                round,
                snap.latest_plan.as_ref(),
                snap.latest_critique.as_ref(),
                &snap.open_issues,
                &steering,
                &repo_ctx,
            );
            let (run_id, (plan, hash)) = match run_and_parse(
                &ctx,
                &prompt,
                plan_contract_json_schema(),
                validate_and_normalize_plan,
            )? {
                Parsed::Ok(id, val) => (id, val),
                Parsed::Exit(next) => return Ok(next),
            };
            let payload = json!({
                "round": round, "role": "author", "ok": true, "summary": plan.summary,
                "plan_contract_hash": hash, "plan_contract": plan, "run_id": run_id,
            });
            deps.emit(EventKind::PlanDebateTurn, task_id, payload)?;
            deps.move_to(
                task_id,
                TaskState::Ready,
                None,
                &format!("plan debate author round {round} finished"),
            )?;
            Ok(Next::Ready)
        }
        DebateRole::Challenger => {
            let Some((plan, hash)) = prev_pair.as_ref() else {
                return record_failure(&ctx, None, "challenger requires a prior author plan");
            };
            let prompt = build_challenger_prompt(
                &task,
                round,
                plan,
                &snap.open_issues,
                &steering,
                &repo_ctx,
            );
            let (run_id, critique) = match run_and_parse(
                &ctx,
                &prompt,
                plan_critique_json_schema(),
                validate_plan_critique,
            )? {
                Parsed::Ok(id, val) => (id, val),
                Parsed::Exit(next) => return Ok(next),
            };
            finish_challenger(&ctx, root, &run_id, plan, hash, &critique, &snap)
        }
    }
}

fn run_and_parse<T>(
    ctx: &TurnCtx<'_>,
    base_prompt: &str,
    schema: &str,
    parse: fn(&str) -> Result<T, String>,
) -> PortResult<Parsed<T>> {
    let mut prompt = base_prompt.to_string();
    for attempt in 0..2u8 {
        let (run_id, outcome) = execute_runner(ctx, prompt, schema)?;
        if let Some(next) = handle_limit_or_infra(ctx, &outcome)? {
            return Ok(Parsed::Exit(next));
        }
        if !outcome.succeeded() {
            let err = outcome_failure_text(&outcome);
            return Ok(Parsed::Exit(record_failure(ctx, Some(&run_id), &err)?));
        }
        match parse(&outcome.final_text) {
            Ok(val) => return Ok(Parsed::Ok(run_id, val)),
            Err(err)
                if attempt == 0
                    && worker::ensure_budget(ctx.deps, &ctx.task.root_id)?.check()
                        == BudgetCheck::Ok =>
            {
                let repair = build_plan_format_repair_prompt(ctx.role, &err, schema);
                prompt = format!("{base_prompt}\n\n{repair}");
            }
            Err(err) => return Ok(Parsed::Exit(record_failure(ctx, Some(&run_id), &err)?)),
        }
    }
    Ok(Parsed::Exit(record_failure(ctx, None, "repair exhausted")?))
}

fn finish_challenger(
    ctx: &TurnCtx<'_>,
    root: &Path,
    run_id: &str,
    plan: &PlanContract,
    hash: &str,
    critique: &PlanCritique,
    snap: &DebateSnapshot,
) -> PortResult<Next> {
    let eval = evaluate_critique(
        ctx.task,
        root,
        &ctx.deps.settings.repo,
        ctx.round,
        plan,
        critique,
        &snap.open_issues,
        &snap.round_issue_sets,
    );
    let issue_set: Vec<(String, String)> = eval.issue_set.iter().cloned().collect();
    let turn_payload = json!({
        "round": ctx.round, "role": "challenger", "ok": true, "verdict": critique.verdict,
        "converged": eval.converged, "stalled": eval.stalled,
        "stall_reason": eval.stall_reason,
        "open_count": eval.open_issues.len(), "open_issues": eval.open_issues,
        "issue_set": issue_set, "dropped_locations": eval.dropped_locations,
        "failed_checks": eval.failed_checks, "critique": critique, "run_id": run_id,
    });
    ctx.deps
        .emit(EventKind::PlanDebateTurn, &ctx.task.id, turn_payload)?;
    if eval.converged {
        let conv = json!({
            "summary": plan.summary, "plan_contract_hash": hash, "plan_contract": plan,
            "run_id": run_id, "round": ctx.round,
        });
        ctx.deps
            .emit(EventKind::PlanConverged, &ctx.task.id, conv)?;
        if ctx.deps.settings.repo.pipeline.require_plan_approval {
            emit_plan_generated(ctx.deps, &ctx.task.id, plan, hash, Some(run_id))?;
            let why = format!(
                "{PLAN_APPROVAL_PREFIX} {}",
                unblock::one_line(&plan.summary, 200)
            );
            unblock::block_untried(ctx.deps, &ctx.task.id, &why)?;
            return Ok(Next::Blocked);
        }
        let note = format_plan_approved_note(&plan.summary, hash);
        ctx.deps
            .store
            .append_context_note(&ctx.task.id, &note, ctx.deps.now())?;
        ctx.deps.move_to(
            &ctx.task.id,
            TaskState::Ready,
            None,
            &format!("plan debate converged at round {}", ctx.round),
        )?;
        return Ok(Next::Ready);
    }
    if eval.stalled {
        emit_plan_generated(ctx.deps, &ctx.task.id, plan, hash, Some(run_id))?;
        let reason = eval
            .stall_reason
            .as_deref()
            .unwrap_or("stalled without convergence");
        unblock::block_untried(
            ctx.deps,
            &ctx.task.id,
            &format!("{PLAN_DEBATE_STALLED_PREFIX} {reason}"),
        )?;
        return Ok(Next::Blocked);
    }
    ctx.deps.move_to(
        &ctx.task.id,
        TaskState::Ready,
        None,
        &format!(
            "plan debate challenger round {} requested revisions",
            ctx.round
        ),
    )?;
    Ok(Next::Ready)
}

fn emit_plan_generated(
    deps: &Deps,
    task_id: &str,
    plan: &PlanContract,
    hash: &str,
    run_id: Option<&str>,
) -> PortResult<()> {
    deps.emit(
        EventKind::PlanGenerated,
        task_id,
        json!({ "summary": plan.summary, "plan_contract_hash": hash, "plan_contract": plan, "run_id": run_id }),
    )
}

fn execute_runner(
    ctx: &TurnCtx<'_>,
    prompt: String,
    schema: &str,
) -> PortResult<(String, AgentOutcome)> {
    let mut run = worker::begin_run_with(
        ctx.deps,
        ctx.task,
        ctx.tree,
        PLAN_EXECUTOR,
        ctx.target.provider_kind,
        ctx.grant,
        ctx.target.hook_required,
        &*ctx.target.accounts,
    )?;
    run.request.sandbox.read_only = true;
    run.request.prompt = prompt;
    run.request.json_schema = Some(schema.to_string());
    let outcome = match ctx.target.runner.run(&run.request) {
        Ok(o) => o,
        Err(err) => {
            worker::end_run(
                ctx.deps,
                ctx.task,
                &run,
                RunStatus::Failed,
                None,
                (0, 0),
                false,
            )?;
            return Err(err);
        }
    };
    worker::end_run(
        ctx.deps,
        ctx.task,
        &run,
        worker::status_of(&outcome),
        outcome.exit_code,
        (outcome.tokens_in, outcome.tokens_out),
        ctx.target.runner.reports_tokens(),
    )?;
    Ok((run.request.run_id, outcome))
}

fn handle_limit_or_infra(ctx: &TurnCtx<'_>, outcome: &AgentOutcome) -> PortResult<Option<Next>> {
    if let Some(hit) = &outcome.limit_hit {
        let pool = &*ctx.target.accounts;
        audit_backend::report_audit_limit(ctx.deps, pool, &ctx.task.id, ctx.grant, hit)?;
        ctx.deps.move_to(
            &ctx.task.id,
            TaskState::Ready,
            None,
            "plan debate turn hit usage limit",
        )?;
        return Ok(Some(audit_backend::route_audit_limit(
            pool, ctx.grant, hit,
        )?));
    }
    if let Some(infra) = &outcome.infra {
        return Ok(Some(handle_plan_infra(ctx, infra)?));
    }
    Ok(None)
}

fn handle_plan_infra(ctx: &TurnCtx<'_>, infra: &InfraFailure) -> PortResult<Next> {
    worker::halt_on_infra_with(
        ctx.deps,
        &ctx.task.id,
        ctx.grant,
        &*ctx.target.accounts,
        infra,
    )
}

fn record_failure(ctx: &TurnCtx<'_>, run_id: Option<&str>, error: &str) -> PortResult<Next> {
    let clean = unblock::one_line(error, 300);
    ctx.deps.emit(
        EventKind::PlanDebateTurn,
        &ctx.task.id,
        json!({ "round": ctx.round, "role": ctx.role, "ok": false, "error": clean, "run_id": run_id }),
    )?;
    let fails = ctx.prior_fails.saturating_add(1);
    if fails >= MAX_CONSECUTIVE_TURN_FAILS {
        if let Some((plan, hash)) = ctx.fallback_plan {
            emit_plan_generated(ctx.deps, &ctx.task.id, plan, hash, run_id)?;
        }
        let why = format!(
            "{PLAN_DEBATE_STALLED_PREFIX} {fails} consecutive plan debate turn failures ({} round {}: {clean})",
            ctx.role, ctx.round
        );
        unblock::block_untried(ctx.deps, &ctx.task.id, &why)?;
        return Ok(Next::Blocked);
    }
    let why = format!(
        "plan debate {} round {} failed; retrying",
        ctx.role, ctx.round
    );
    ctx.deps
        .move_to(&ctx.task.id, TaskState::Ready, None, &why)?;
    Ok(Next::Ready)
}

fn outcome_failure_text(outcome: &AgentOutcome) -> String {
    if outcome.timed_out {
        return "plan debate turn timed out".to_string();
    }
    let tail = unblock::one_line(&outcome.log_tail, 240);
    match outcome.exit_code {
        Some(code) if !tail.is_empty() => format!("exit code {code}: {tail}"),
        Some(code) => format!("exit code {code}"),
        None if !tail.is_empty() => tail,
        None => "plan debate turn failed without exit code".to_string(),
    }
}
