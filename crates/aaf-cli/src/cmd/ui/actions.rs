//! Task mutation and formatting helpers for `aaf ui`.

use aaf_domain::blocked::BlockedReason;
use aaf_domain::ports::Clock;
use aaf_engine::db::Db;
use serde_json::{Value, json};

use crate::cmd::task_ops::{parse_rework_reason, rework_task, unblock_task};
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
