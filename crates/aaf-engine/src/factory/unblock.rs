//! Runs the unblocker ladder for a failed step and executes its decision.
//! The ladder (`crate::unblocker`) only decides; everything it decides is
//! carried out here.

use aaf_domain::blocked::{BlockedReason, SubtaskPlan, UnblockAction};
use aaf_domain::budget::BudgetCheck;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{AgentRunner, LimitHit, PortResult};
use aaf_domain::task::{ImmutableSpec, Task, TaskState};
use serde_json::json;

use super::adapters::provider_for;
use super::audit_turn::InfraWatch;
use super::ports::{AccountPool, Grant};
use super::stage_router::StageTarget;
use super::worker::{GateJob, Job, Next, ensure_budget, halt_on_infra_with, hook_problem_if};
use super::{Deps, Store};
use crate::git::TaskWorktree;
use crate::git::worktree::validate_task_id;
use crate::unblocker::{FailureSignal, Ladder, QuotaRoute, failure_digest};
use crate::webhook::WebhookEvent;

mod audit_rework;
mod proposer;
#[cfg(test)]
pub(crate) use audit_rework::audit_fails_since_reset;
pub(crate) use audit_rework::{
    format_sanitized_audit_findings, handle_audit_failure, sanitize_untrusted_close_tag,
};
use proposer::LazyProposer;

/// Records an account rate-limit cooldown on `accounts` and emits `AccountCooldown`.
pub(super) fn report_limit_on(
    deps: &Deps,
    task_id: &str,
    grant: &Grant,
    accounts: &dyn AccountPool,
    hit: &LimitHit,
) -> PortResult<()> {
    if let Some(account) = grant.account_id() {
        accounts.report_limit(account, hit)?;
        deps.emit(
            EventKind::AccountCooldown,
            task_id,
            json!({
                "account": account,
                "reset_at": hit.reset_at,
                "raw": one_line(&hit.raw, 300),
            }),
        )?;
    }
    Ok(())
}

/// Decides whether another account on `accounts` may take over after a limit
/// hit or whether the task must wait for cooldown.
pub(super) fn route_limit_hit_on(
    grant: &Grant,
    accounts: &dyn AccountPool,
    hit: &LimitHit,
) -> PortResult<Next> {
    let wait = Ok(Next::Wait {
        until: hit.reset_at,
    });
    if grant.account_id().is_none() {
        return wait;
    }
    match accounts.after_limit()? {
        Some(QuotaRoute::Switch) => Ok(Next::Switch),
        None if accounts.provider_account_count()? > 1 => Ok(Next::Switch),
        Some(QuotaRoute::Wait) | None => wait,
    }
}

/// A rebase that conflicts sends the failure through the ladder again; the
/// second pass finds T2 already tried, so this bound is never the stop.
const MAX_LADDER_PASSES: usize = 3;
const SUMMARY_MAX_CHARS: usize = 500;

/// 32 hex characters, unique per call: run ids, webhook event ids and
/// integration-tree names. Not a secret; it only has to never repeat, across
/// processes too, which the pid, the time and the counter give. The
/// OS-seeded hasher adds spread without a random-number dependency.
pub(crate) fn unique_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let seed = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let material = format!("{}:{nanos}:{count}:{seed}", std::process::id());
    let mut id = aaf_fp::sha256_hex(material.as_bytes());
    id.truncate(32);
    id
}

pub(crate) fn one_line(text: &str, max_chars: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    flat.chars().take(max_chars).collect()
}

pub(crate) fn ladder(deps: &Deps) -> Ladder<'_> {
    let s: &dyn Store = &*deps.store;
    Ladder::new(s, s, s, s, &*deps.clock, &deps.settings.repo.unblocker)
}

pub(crate) fn is_human_unblock_event(event: &FactoryEvent) -> bool {
    crate::plan_debate::state::is_human_unblock_event(event)
}

/// Consecutive earlier unblock steps of the task with this digest.
fn prior_same(deps: &Deps, task_id: &str, digest: &str) -> PortResult<u32> {
    let mut count = 0;
    let kinds = [
        EventKind::UnblockStep,
        EventKind::TaskStateChanged,
        EventKind::ReworkRequested,
    ];
    for event in deps
        .store
        .events_for_task_by_kinds(task_id, &kinds, u32::MAX)?
    {
        if event.kind == EventKind::ReworkRequested || is_human_unblock_event(&event) {
            break;
        }
        if event.kind != EventKind::UnblockStep {
            continue;
        }
        let same = serde_json::from_str::<serde_json::Value>(&event.payload_json)
            .ok()
            .is_some_and(|p| p["digest"].as_str() == Some(digest));
        if !same {
            break;
        }
        count += 1;
    }
    Ok(count)
}

/// True when a human unblock event is newer than any `UnblockStep` or
/// `ReworkRequested` event while stale `unblock[` markers remain after the
/// latest `rework:`/`unblocked:` note.
fn has_unrecorded_human_unblock(deps: &Deps, task: &Task) -> PortResult<bool> {
    let notes = &task.context_notes;
    let start = notes
        .iter()
        .rposition(|n| n.starts_with("rework:") || n.starts_with("unblocked:"))
        .map_or(0, |idx| idx + 1);
    if !notes[start..].iter().any(|n| n.starts_with("unblock[")) {
        return Ok(false);
    }
    let kinds = [
        EventKind::UnblockStep,
        EventKind::TaskStateChanged,
        EventKind::ReworkRequested,
    ];
    for event in deps
        .store
        .events_for_task_by_kinds(&task.id, &kinds, u32::MAX)?
    {
        if is_human_unblock_event(&event) {
            return Ok(true);
        }
        if event.kind == EventKind::UnblockStep || event.kind == EventKind::ReworkRequested {
            return Ok(false);
        }
    }
    Ok(false)
}

/// Classifies the failure, asks the ladder and carries out its decision.
#[allow(dead_code)]
pub(crate) fn handle(
    deps: &Deps,
    task_id: &str,
    worktree: Option<&TaskWorktree>,
    signal: FailureSignal,
    grant: &Grant,
) -> PortResult<Next> {
    handle_with(deps, task_id, worktree, signal, grant, None)
}

/// Classifies the failure, asks the ladder and carries out its decision using
/// `stage`'s provider target (runner, account pool, hook policy) when present.
pub(crate) fn handle_with(
    deps: &Deps,
    task_id: &str,
    worktree: Option<&TaskWorktree>,
    signal: FailureSignal,
    grant: &Grant,
    stage: Option<&StageTarget>,
) -> PortResult<Next> {
    handle_with_opt(deps, task_id, worktree, signal, Some(grant), stage, false)
}

/// Like [`handle_with`], but accepts `grant: None` for deterministic unblocks
/// so T2/T3/Freeze/Escalate run without holding an account lease and T4 acquires lazily.
pub(crate) fn handle_with_opt(
    deps: &Deps,
    task_id: &str,
    worktree: Option<&TaskWorktree>,
    mut signal: FailureSignal,
    grant: Option<&Grant>,
    stage: Option<&StageTarget>,
    provider_cooling: bool,
) -> PortResult<Next> {
    let (runner, accounts, provider_kind, hook_req): (&dyn AgentRunner, &dyn AccountPool, _, bool) =
        match stage {
            Some(t) => (&*t.runner, &*t.accounts, t.provider_kind, t.hook_required),
            None => (
                &*deps.runner,
                &*deps.accounts,
                provider_for(&deps.settings.repo.run.executor),
                deps.settings.hook_required,
            ),
        };
    let cwd = worktree.map_or_else(
        || deps.settings.repo_root.clone(),
        |w| w.path.to_string_lossy().into_owned(),
    );
    let branch = worktree.map_or_else(String::new, |w| w.branch.clone());
    let exec = stage.map_or(deps.settings.repo.run.executor.as_str(), |t| {
        t.executor.as_str()
    });
    if let Some(problem) = worktree.and_then(|w| hook_problem_if(deps, &w.path, hook_req, exec)) {
        // The run that just failed changed its own guard (or the base lost
        // it): no further agent starts in this worktree, neither a retry
        // nor the subtask proposer. A person looks at it; what failed is
        // told by the run, and the block itself is not a failed root.
        block_untried(deps, task_id, &problem)?;
        return Ok(Next::Blocked);
    }
    for _ in 0..MAX_LADDER_PASSES {
        let mut task = deps.store.get_task(task_id)?;
        if has_unrecorded_human_unblock(deps, &task)? {
            deps.store
                .append_context_note(&task.id, "unblocked: human unblock", deps.now())?;
            task = deps.store.get_task(task_id)?;
        }
        signal.budget = ensure_budget(deps, &task.root_id)?.check();
        let was_frozen = signal.budget == BudgetCheck::Frozen;
        signal.same_failure_count = prior_same(deps, task_id, &failure_digest(&signal))? + 1;
        let watch = InfraWatch::new(runner);
        let proposer = LazyProposer::new(
            deps,
            &task,
            &cwd,
            &branch,
            provider_kind,
            hook_req,
            accounts,
            &watch,
            grant,
            provider_cooling,
        );
        // Asked after the limit was reported, so the account that hit it
        // already counts as cooling down.
        let route = accounts.after_limit()?;
        let decision = ladder(deps).next_action_routed(task_id, &signal, Some(&proposer), route)?;
        let account_busy = proposer.was_account_busy();
        let lazy_grant = proposer.take_grant();
        drop(proposer);
        let effective_grant = grant.or(lazy_grant.as_ref());
        let (proposer_infra, proposer_limit) = watch.take();
        if let Some(hit) = proposer_limit
            && let Some(active_grant) = effective_grant
        {
            report_limit_on(deps, task_id, active_grant, accounts, &hit)?;
            deps.move_to(
                task_id,
                TaskState::Ready,
                None,
                "subtask proposer hit usage limit; waiting for account",
            )?;
            return route_limit_hit_on(active_grant, accounts, &hit);
        }
        if let Some(infra) = proposer_infra
            && let Some(active_grant) = effective_grant
        {
            return halt_on_infra_with(deps, task_id, active_grant, accounts, &infra);
        }
        if account_busy && let Some(wt) = worktree {
            return Ok(Next::RetryAudit(Box::new(Job::Unblock {
                job: GateJob {
                    task_id: task_id.to_string(),
                    worktree: wt.clone(),
                    audited: false,
                    cached_receipt: None,
                },
                signal: Box::new(signal),
                needs_account: true,
            })));
        }
        let why = format!(
            "unblock {}: {}",
            decision.tier.as_str(),
            decision.reason.kind()
        );
        match decision.action {
            // The account is already cooling down (reported when the limit
            // was hit). Back in Ready the task starts on another account.
            UnblockAction::SwitchAccount => {
                deps.move_to(task_id, TaskState::Ready, None, &why)?;
                if effective_grant.and_then(Grant::account_id).is_none() {
                    let until = match decision.reason {
                        BlockedReason::Quota { reset_at } => reset_at,
                        _ => None,
                    };
                    return Ok(Next::Wait { until });
                }
                return Ok(Next::Switch);
            }
            // No other account may take it (a pinned repository, or all of
            // them limited): Ready, and waiting for the cooldown. Waiting is
            // not a failure.
            UnblockAction::WaitForAccount { until } => {
                let why = match until {
                    Some(until) => format!("{why}; waiting for the account until {until}"),
                    None => format!("{why}; waiting for the account's cooldown"),
                };
                deps.move_to(task_id, TaskState::Ready, None, &why)?;
                return Ok(Next::Wait { until });
            }
            UnblockAction::WaitForDependency { .. } | UnblockAction::RetryFreshContext { .. } => {
                deps.move_to(task_id, TaskState::Ready, None, &why)?;
                return Ok(Next::Ready);
            }
            UnblockAction::RetryInfra { .. } => {
                deps.move_to(task_id, TaskState::Ready, None, &why)?;
                return Ok(Next::Wait {
                    until: Some(deps.now() + 15),
                });
            }
            UnblockAction::RerunTests => return regate(deps, &task, worktree, &why),
            UnblockAction::RebaseAndRegate => {
                let Some(worktree) = worktree else {
                    deps.move_to(task_id, TaskState::Ready, None, &why)?;
                    return Ok(Next::Ready);
                };
                let files = deps.git.rebase(worktree)?;
                if files.is_empty() {
                    return regate(deps, &task, Some(worktree), &why);
                }
                signal = FailureSignal::empty();
                signal.merge_conflict_files = files;
            }
            UnblockAction::SpawnSubtasks { plans } => {
                spawn_subtasks(deps, &task, plans)?;
                // The new dependencies hold the parent back until every
                // subtask is done.
                deps.move_to(task_id, TaskState::Ready, None, &why)?;
                return Ok(Next::Ready);
            }
            UnblockAction::EscalateHuman { why } => {
                if matches!(decision.reason, BlockedReason::Infra { .. }) {
                    // Upstream infra failures must never count toward the global freeze.
                    super::infra_retry::block_infra(deps, &task, &decision.reason, &why)?;
                } else if matches!(decision.reason, BlockedReason::AuditRejected { .. }) {
                    // Audit/score-gate exhaustion hands over to human review without freezing the factory.
                    block(deps, &task, &decision.reason, "audit_rejected", &why, false)?;
                } else {
                    let reason = BlockedReason::NeedsHuman { why: why.clone() };
                    block(deps, &task, &reason, "needs_human", &why, true)?;
                }
                return Ok(Next::Blocked);
            }
            UnblockAction::FreezeRoot { why } => {
                let reason = BlockedReason::BudgetExceeded {
                    detail: why.clone(),
                };
                block(deps, &task, &reason, "budget_frozen", &why, !was_frozen)?;
                return Ok(Next::Blocked);
            }
        }
    }
    deps.move_to(task_id, TaskState::Ready, None, "rebase keeps conflicting")?;
    Ok(Next::Ready)
}

fn regate(
    deps: &Deps,
    task: &Task,
    worktree: Option<&TaskWorktree>,
    why: &str,
) -> PortResult<Next> {
    let Some(worktree) = worktree else {
        deps.move_to(&task.id, TaskState::Ready, None, why)?;
        return Ok(Next::Ready);
    };
    if task.state != TaskState::Gating {
        deps.move_to(&task.id, TaskState::Gating, None, why)?;
    }
    Ok(Next::Gate(GateJob {
        task_id: task.id.clone(),
        worktree: worktree.clone(),
        audited: false,
        cached_receipt: None,
    }))
}

fn plans_share_files(parent: &Task, plans: &[SubtaskPlan]) -> bool {
    if plans.len() < 2 {
        return false;
    }
    if !parent.scope_paths.is_empty() {
        return true;
    }
    let mut seen = std::collections::HashSet::new();
    for plan in plans {
        let mut plan_paths = std::collections::HashSet::new();
        for token in plan
            .title
            .split_whitespace()
            .chain(plan.description.split_whitespace())
        {
            let trimmed = token.trim_matches(|c: char| {
                matches!(
                    c,
                    '`' | '\'' | '"' | ',' | ';' | ':' | '(' | ')' | '[' | ']' | '.'
                )
            });
            if trimmed.contains('/')
                && trimmed.contains('.')
                && !trimmed.starts_with("http")
                && !trimmed.starts_with('/')
            {
                plan_paths.insert(trimmed.to_string());
            }
        }
        for path in plan_paths {
            if !seen.insert(path) {
                return true;
            }
        }
    }
    false
}

fn spawn_subtasks(deps: &Deps, parent: &Task, plans: Vec<SubtaskPlan>) -> PortResult<()> {
    let existing = deps.store.children_of(&parent.id)?.len();
    let chain_sequentially = plans_share_files(parent, &plans);
    let mut prev_id: Option<String> = None;
    for (index, plan) in plans.into_iter().enumerate() {
        let mut id = format!("{}-s{}", parent.id, existing + index + 1);
        if validate_task_id(&id).is_err() {
            // The parent id left no room for a suffix.
            id = format!("s-{}", unique_id());
        }
        let spec = ImmutableSpec {
            description: plan.description,
            acceptance: plan.acceptance,
        };
        let child = Task::new_subtask(id, parent, plan.title, spec, deps.now())
            .map_err(|e| aaf_domain::ports::PortError::Invalid(e.to_string()))?;
        deps.store.insert_task(&child)?;
        deps.store.add_dep(&parent.id, &child.id)?;
        if chain_sequentially && let Some(ref prev) = prev_id {
            deps.store.add_dep(&child.id, prev)?;
        }
        prev_id = Some(child.id.clone());
        deps.emit(
            EventKind::TaskCreated,
            &child.id,
            json!({ "parent": parent.id, "title": child.title }),
        )?;
    }
    Ok(())
}

/// T5: the task stops, a human is told, and with `counts` the failed root
/// counts towards the global freeze.
pub(super) fn block(
    deps: &Deps,
    task: &Task,
    reason: &BlockedReason,
    status: &str,
    why: &str,
    counts: bool,
) -> PortResult<()> {
    deps.move_to(&task.id, TaskState::Blocked, Some(reason), why)?;
    let event = WebhookEvent {
        event_id: unique_id(),
        kind: EventKind::UnblockStep,
        task_id: Some(task.id.clone()),
        status: status.to_string(),
        summary: one_line(&format!("{}: {why}", task.title), SUMMARY_MAX_CHARS),
        created_at: deps.now(),
    };
    // A failed notification must not undo the block: it is recorded and the
    // task stays where the board shows it.
    match deps.notifier.notify(&event) {
        Ok(true) => deps.emit(EventKind::WebhookSent, &task.id, json!({ "ok": true }))?,
        Ok(false) => {}
        Err(e) => deps.emit(
            EventKind::WebhookSent,
            &task.id,
            json!({ "ok": false, "error": e }),
        )?,
    }
    if counts {
        ladder(deps).record_root_result(&task.root_id, false)?;
    }
    Ok(())
}

/// Blocks a task that was never tried because its worktree is not fit to
/// start an agent in (the hook registration) or because the gate could not
/// verify/run. A human is told, but it does not count towards the global
/// freeze, which would stop every repository for one repo's configuration.
pub(crate) fn block_untried(deps: &Deps, task_id: &str, why: &str) -> PortResult<()> {
    let task = deps.store.get_task(task_id)?;
    let reason = BlockedReason::NeedsHuman {
        why: why.to_string(),
    };
    block(deps, &task, &reason, "needs_human", why, false)
}

/// A root task that reached its merge point resets the consecutive-failure
/// counter; subtasks do not count on their own.
pub(crate) fn root_succeeded(deps: &Deps, task: &Task) -> PortResult<()> {
    if task.id == task.root_id {
        ladder(deps).record_root_result(&task.root_id, true)?;
    }
    Ok(())
}
