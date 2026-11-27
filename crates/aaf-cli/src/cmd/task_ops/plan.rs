//! Epoch-aware Plan Debate preservation for human `unblock` operations.

use aaf_domain::blocked::BlockedReason;
use aaf_domain::event::EventKind;
use aaf_domain::ports::{EventSink, PortError, TaskStore};
use aaf_domain::task::Task;
use aaf_engine::db::Db;
use aaf_engine::factory::prompt::{PLAN_APPROVED_PREFIX, STEER_PREFIX};
use aaf_engine::plan_debate::PLAN_DEBATE_STALLED_PREFIX;
use aaf_engine::plan_debate::schema::format_plan_approved_note;
use aaf_engine::plan_debate::state::replay_debate_state;

use super::{
    LatestPlan, PLAN_APPROVAL_PREFIX, format_approved_plan_note, latest_plan_info,
    plan_from_task_notes,
};

const DEBATE_KINDS: &[EventKind] = &[
    EventKind::PlanConverged,
    EventKind::PlanDebateTurn,
    EventKind::TaskStateChanged,
    EventKind::ReworkRequested,
];

fn has_steer_after_last_plan(task: &Task) -> bool {
    for note in task.context_notes.iter().rev() {
        if note.starts_with(PLAN_APPROVED_PREFIX) || note.starts_with("unblocked:") {
            return false;
        }
        if note.starts_with(STEER_PREFIX) {
            return true;
        }
    }
    false
}

pub(super) fn maybe_approve_on_unblock(
    db: &Db,
    task: &Task,
    now: i64,
) -> Result<(bool, Option<LatestPlan>), PortError> {
    if let Some(BlockedReason::NeedsHuman { why }) = &task.blocked_reason {
        if let Some(rest) = why.strip_prefix(PLAN_APPROVAL_PREFIX) {
            if let Some(info) = latest_plan_info(db, &task.id)? {
                db.append_context_note(&task.id, &format_approved_plan_note(&info), now)?;
                return Ok((true, info.plan_contract.is_some().then_some(info)));
            }
            let plan = if rest.trim().is_empty() { why } else { rest.trim() };
            db.append_context_note(&task.id, &format!("{PLAN_APPROVED_PREFIX} {plan}"), now)?;
            return Ok((true, None));
        }
        if why.starts_with(PLAN_DEBATE_STALLED_PREFIX) {
            return Ok((false, None));
        }
    }
    if !has_steer_after_last_plan(task) {
        let events = db.events_for_task_by_kinds(&task.id, DEBATE_KINDS, u32::MAX)?;
        let snap = replay_debate_state(&events);
        if let Some((contract, hash)) = snap.converged {
            let summary = contract.summary.trim().to_string();
            let full = format_plan_approved_note(&summary, &hash);
            let note_body = full
                .strip_prefix(PLAN_APPROVED_PREFIX)
                .map_or(full.as_str(), str::trim)
                .to_string();
            return Ok((
                true,
                Some(LatestPlan {
                    note_body,
                    summary,
                    plan_contract_hash: Some(hash),
                    plan_contract: Some(contract),
                }),
            ));
        }
        if matches!(task.blocked_reason, Some(BlockedReason::BudgetExceeded { .. }))
            && snap.latest_plan.is_none()
            && plan_from_task_notes(task).is_some()
        {
            return Ok((true, None));
        }
    }
    Ok((false, None))
}
