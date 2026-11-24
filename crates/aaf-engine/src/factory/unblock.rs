//! Runs the unblocker ladder for a failed step and executes its decision.
//! The ladder (`crate::unblocker`) only decides; everything it decides is
//! carried out here.

use aaf_domain::blocked::{BlockedReason, SubtaskPlan, UnblockAction};
use aaf_domain::budget::BudgetCheck;
use aaf_domain::config::MAX_AUDIT_REWORKS_CAP;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{LimitHit, PortResult};
use aaf_domain::task::{ImmutableSpec, Task, TaskState};
use serde_json::json;

use super::audit_turn::InfraWatch;
use super::ports::Grant;
use super::rework_filter::is_valid_rework_finding;
use super::worker::{GateJob, Next, ensure_budget, halt_on_infra, hook_problem, request};
use super::{Deps, Store};
use crate::cross_audit::Finding;
use crate::git::TaskWorktree;
use crate::git::worktree::validate_task_id;
use crate::unblocker::{AgentSubtaskProposer, FailureSignal, Ladder, QuotaRoute, failure_digest};
use crate::webhook::WebhookEvent;

mod proposer;
use proposer::ProposerRecorder;

/// Records an account rate-limit cooldown and emits `AccountCooldown`.
pub(super) fn report_account_limit(
    deps: &Deps,
    task_id: &str,
    grant: &Grant,
    hit: &LimitHit,
) -> PortResult<()> {
    if let Some(account) = grant.account_id() {
        deps.accounts.report_limit(account, hit)?;
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

/// Decides whether another account may take over after a limit hit or whether
/// the task must wait for cooldown.
pub(super) fn route_limit_hit(deps: &Deps, grant: &Grant, hit: &LimitHit) -> PortResult<Next> {
    let wait = Ok(Next::Wait {
        until: hit.reset_at,
    });
    if grant.account_id().is_none() {
        return wait;
    }
    match deps.accounts.after_limit()? {
        Some(QuotaRoute::Switch) => Ok(Next::Switch),
        None if deps.accounts.provider_account_count()? > 1 => Ok(Next::Switch),
        Some(QuotaRoute::Wait) | None => wait,
    }
}

/// A rebase that conflicts sends the failure through the ladder again; the
/// second pass finds T2 already tried, so this bound is never the stop.
const MAX_LADDER_PASSES: usize = 3;
const SUMMARY_MAX_CHARS: usize = 500;
const AUDIT_FINDINGS_MAX_CHARS: usize = 1500;
const HISTORY_EVENTS: u32 = 50;

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

fn is_human_unblock_event(event: &FactoryEvent) -> bool {
    event.kind == EventKind::TaskStateChanged
        && serde_json::from_str::<serde_json::Value>(&event.payload_json)
            .ok()
            .is_some_and(|p| {
                p["to"].as_str() == Some("ready")
                    && p["why"]
                        .as_str()
                        .is_some_and(|w| w == "unblocked" || w.ends_with(" unblock"))
            })
}

/// Consecutive earlier unblock steps of the task with this digest.
fn prior_same(deps: &Deps, task_id: &str, digest: &str) -> PortResult<u32> {
    let mut count = 0;
    for event in deps.store.events_for_task(task_id, HISTORY_EVENTS)? {
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
    for event in deps.store.events_for_task(&task.id, HISTORY_EVENTS)? {
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
pub(crate) fn handle(
    deps: &Deps,
    task_id: &str,
    worktree: Option<&TaskWorktree>,
    mut signal: FailureSignal,
    grant: &Grant,
) -> PortResult<Next> {
    let cwd = worktree.map_or_else(
        || deps.settings.repo_root.clone(),
        |w| w.path.to_string_lossy().into_owned(),
    );
    let branch = worktree.map_or_else(String::new, |w| w.branch.clone());
    if let Some(problem) = worktree.and_then(|w| hook_problem(deps, &w.path)) {
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
        signal.same_failure_count = prior_same(deps, task_id, &failure_digest(&signal))? + 1;
        let watch = InfraWatch::new(&*deps.runner);
        let recorder = ProposerRecorder {
            deps,
            task: &task,
            branch: branch.clone(),
            account_id: grant.account_id(),
            inner: &watch,
        };
        let mut proposer_base = request(deps, &cwd, grant)?;
        proposer_base.sandbox.read_only = true;
        let proposer = AgentSubtaskProposer {
            runner: &recorder,
            budgets: &*deps.store as &dyn Store,
            base: proposer_base,
        };
        // Asked after the limit was reported, so the account that hit it
        // already counts as cooling down.
        let route = deps.accounts.after_limit()?;
        let decision = ladder(deps).next_action_routed(task_id, &signal, Some(&proposer), route)?;
        drop(proposer);
        drop(recorder);
        let (proposer_infra, proposer_limit) = watch.take();
        if let Some(hit) = proposer_limit {
            report_account_limit(deps, task_id, grant, &hit)?;
            deps.move_to(
                task_id,
                TaskState::Ready,
                None,
                "subtask proposer hit usage limit; waiting for account",
            )?;
            return route_limit_hit(deps, grant, &hit);
        }
        if let Some(infra) = proposer_infra {
            return halt_on_infra(deps, task_id, grant, &infra);
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
                if grant.account_id().is_none() {
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
                    block(deps, &task, &decision.reason, "infra", &why, false)?;
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
                block(deps, &task, &reason, "budget_frozen", &why, true)?;
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

fn spawn_subtasks(deps: &Deps, parent: &Task, plans: Vec<SubtaskPlan>) -> PortResult<()> {
    let existing = deps.store.children_of(&parent.id)?.len();
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
fn block(
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

/// Neutralises any closing `</untrusted-data` tag (ASCII case-insensitive) so
/// untrusted audit findings cannot escape their wrapper block.
pub(super) fn sanitize_untrusted_close_tag(text: &str) -> String {
    const NEEDLE: &[u8] = b"</untrusted-data";
    let (bytes, mut out, mut copied, mut at) =
        (text.as_bytes(), String::with_capacity(text.len()), 0, 0);
    while at + NEEDLE.len() <= bytes.len() {
        if bytes[at..at + NEEDLE.len()].eq_ignore_ascii_case(NEEDLE) {
            out.push_str(&text[copied..at]);
            out.push_str("&lt;/untrusted-data");
            at += NEEDLE.len();
            copied = at;
        } else {
            at += 1;
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// Counts explicit failing `CrossAudit` events for `task_id` since the latest
/// `ReworkRequested` or human unblock (`Blocked -> Ready`) using an unbounded
/// reverse scan so counts are never dropped past 50 events.
pub(crate) fn audit_fails_since_reset(deps: &Deps, task_id: &str) -> PortResult<u32> {
    let mut count: u32 = 0;
    for event in deps.store.events_for_task(task_id, u32::MAX)? {
        if event.kind == EventKind::ReworkRequested || is_human_unblock_event(&event) {
            break;
        }
        if event.kind == EventKind::CrossAudit
            && serde_json::from_str::<serde_json::Value>(&event.payload_json)
                .ok()
                .is_some_and(|p| {
                    p["verdict"]
                        .as_str()
                        .is_some_and(|v| v == "fail" || v.starts_with("fail:"))
                })
        {
            count = count.saturating_add(1);
        }
    }
    Ok(count)
}

/// Automatic audit-rework routing on explicit `AuditVerdict::Fail` (never
/// `Inconclusive` or truncated diffs). Bounded by `max_audit_reworks` and
/// token/wall-clock budget without consuming `RootBudget.cycles_used`.
pub(crate) fn handle_audit_failure(
    deps: &Deps,
    task: &Task,
    findings: &[Finding],
) -> PortResult<Option<Next>> {
    let cap = deps
        .settings
        .repo
        .pipeline
        .max_audit_reworks
        .min(MAX_AUDIT_REWORKS_CAP);
    let prior_audit_fails = audit_fails_since_reset(deps, &task.id)?.saturating_sub(1);
    if cap == 0 || prior_audit_fails >= cap {
        return Ok(None);
    }
    let (valid, dropped): (Vec<_>, Vec<_>) =
        findings.iter().partition(|f| is_valid_rework_finding(f));
    if !dropped.is_empty() {
        let items: Vec<_> = dropped
            .iter()
            .map(|f| {
                let reason = if f.evidence.trim().is_empty() {
                    "empty_evidence"
                } else {
                    "missing_file_line"
                };
                json!({ "id": f.id, "reason": reason })
            })
            .collect();
        deps.emit(
            EventKind::DetectorHit,
            &task.id,
            json!({ "source": "audit_rework_filter", "dropped_count": items.len(), "dropped": items }),
        )?;
    }
    if valid.is_empty() {
        return Ok(None);
    }
    let budget = ensure_budget(deps, &task.root_id)?;
    if matches!(
        budget.check(),
        BudgetCheck::Frozen | BudgetCheck::TokensExceeded | BudgetCheck::WallClockExceeded
    ) {
        return Ok(None);
    }
    let raw = valid
        .iter()
        .map(|f| format!("{}: {} ({})", f.id, f.question, f.evidence.trim()))
        .collect::<Vec<_>>()
        .join("; ");
    let safe = sanitize_untrusted_close_tag(&one_line(&raw, AUDIT_FINDINGS_MAX_CHARS));
    let note =
        format!("audit_rework: <untrusted-data id=\"audit-findings\">{safe}</untrusted-data>");
    deps.store
        .append_context_note(&task.id, &note, deps.now())?;
    deps.move_to(&task.id, TaskState::Ready, None, "audit_rework")?;
    Ok(Some(Next::Ready))
}
