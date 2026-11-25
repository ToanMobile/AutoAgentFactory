//! Task mutation and formatting helpers for `aaf ui`.

use aaf_domain::blocked::BlockedReason;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{Clock, EventSink, TaskStore};
use aaf_domain::task::TaskState;
use aaf_engine::db::Db;
use serde_json::{Value, json};

use crate::cmd::task_ops::{
    PLAN_APPROVAL_PREFIX, PLAN_DEBATE_STALLED_PREFIX, emit_human_plan_converged,
    format_approved_plan_note, latest_plan_info, parse_rework_reason, reset_exhausted_budget,
    rework_task, unblock_task,
};
use crate::ctx::{Ctx, SystemClock};

const RECENT_RUNS_LIMIT: usize = 20;

pub(super) fn do_unblock(db: &Db, task_id: &str) -> Result<Value, String> {
    unblock_task(db, task_id, "aaf ui unblock", "web-ui", SystemClock.now())
        .map_err(|e| e.to_refused_string())?;
    Ok(json!({ "action": "unblock", "task_id": task_id, "state": "ready" }))
}

pub(super) fn do_rework(db: &Db, ctx: &Ctx, task_id: &str, reason: &str) -> Result<Value, String> {
    let (rerun, cleaned) = parse_rework_reason(reason);
    if cleaned.is_empty() {
        return Err("invalid rework reason: empty".into());
    }
    let out = rework_task(
        db,
        ctx,
        task_id,
        cleaned,
        rerun,
        "aaf ui rework",
        "web-ui",
        SystemClock.now(),
    )
    .map_err(|e| e.to_refused_string())?;
    Ok(json!({ "action": "rework", "task_id": task_id, "state": out.target.as_str() }))
}

pub(super) fn collect_recent_runs(db: &Db, repo_root: Option<&str>) -> Vec<Value> {
    db.recent_runs(repo_root, RECENT_RUNS_LIMIT)
        .unwrap_or_default()
        .into_iter()
        .map(|r| {
            json!({
                "id": r.id, "task_id": r.task_id, "executor": r.executor,
                "status": r.status.as_str(), "started_at": r.started_at, "ended_at": r.ended_at
            })
        })
        .collect()
}

pub(super) fn s_err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

pub(super) fn format_blocked_detail(reason: &BlockedReason) -> String {
    match reason {
        BlockedReason::NeedsHuman { why } => why.clone(),
        BlockedReason::GateFailed { summary } => summary.clone(),
        BlockedReason::Dependency { task_id } => format!("waiting on {task_id}"),
        BlockedReason::MergeConflict { detail }
        | BlockedReason::FlakyTest { detail }
        | BlockedReason::Stuck { detail }
        | BlockedReason::BudgetExceeded { detail }
        | BlockedReason::Infra { detail } => detail.clone(),
        BlockedReason::Quota { reset_at } => match reset_at {
            Some(ts) => format!("quota exhausted (reset_at={ts})"),
            None => "quota exhausted".to_string(),
        },
    }
}

pub(super) fn do_approve_plan(db: &Db, task_id: &str) -> Result<Value, String> {
    let task = db.get_task(task_id).map_err(s_err)?;
    if task.state != TaskState::Blocked {
        return Err(format!(
            "refused: {task_id} is {}, not blocked",
            task.state.as_str()
        ));
    }
    let waiting = matches!(
        &task.blocked_reason,
        Some(BlockedReason::NeedsHuman { why })
            if why.starts_with(PLAN_APPROVAL_PREFIX)
                || why.starts_with(PLAN_DEBATE_STALLED_PREFIX)
    );
    if !waiting {
        return Err(format!(
            "refused: {task_id} is not waiting for plan approval"
        ));
    }
    let Some(plan) = latest_plan_info(db, task_id).map_err(s_err)? else {
        return Err(format!(
            "refused: {task_id} has no generated plan event to approve"
        ));
    };
    let now = SystemClock.now();
    let note = format_approved_plan_note(&plan);
    db.append_context_note(task_id, &note, now).map_err(s_err)?;
    emit_human_plan_converged(db, task_id, &plan, now).map_err(s_err)?;
    let budget_reset = reset_exhausted_budget(db, &task.root_id).map_err(s_err)?;
    db.set_state(task_id, TaskState::Ready, None, now)
        .map_err(s_err)?;
    let payload = json!({
        "from": "blocked", "to": "ready", "why": "plan_approved",
        "by": "web-ui", "budget_reset": budget_reset,
    });
    let ev = FactoryEvent::new(EventKind::TaskStateChanged, payload.to_string(), now);
    db.emit(&ev.for_task(task_id)).map_err(s_err)?;
    Ok(json!({
        "action": "approve_plan", "task_id": task_id, "state": "ready",
        "approved": true, "budget_reset": budget_reset,
    }))
}
