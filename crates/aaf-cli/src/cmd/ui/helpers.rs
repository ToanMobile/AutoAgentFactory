//! Helper functions for `aaf ui` cockpit endpoints.

use std::fs;
use std::path::{Path, PathBuf};

use aaf_domain::blocked::BlockedReason;
use aaf_domain::budget::{BudgetCheck, RootBudget};
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{BudgetStore, EventSink, PortError, RunStore, TaskStore};
use aaf_domain::task::{Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::factory::prompt::PLAN_APPROVED_PREFIX;
use aaf_engine::git::worktree::worktree_path;
use serde_json::{Value, json};

use super::{PLAN_APPROVAL_PREFIX, PLAN_EVENT_LOOKBACK, RECENT_RUNS_LIMIT};
use crate::ctx::Ctx;

pub(super) fn collect_recent_runs(db: &Db, repo_root: Option<&str>) -> Vec<Value> {
    let mut runs: Vec<_> = db
        .list_tasks(repo_root, None)
        .unwrap_or_default()
        .into_iter()
        .flat_map(|t| db.runs_for_task(&t.id).unwrap_or_default())
        .collect();
    runs.sort_by(|a, b| {
        b.started_at
            .cmp(&a.started_at)
            .then_with(|| b.id.cmp(&a.id))
    });
    runs.into_iter()
        .take(RECENT_RUNS_LIMIT)
        .map(|r| json!({ "id": r.id, "task_id": r.task_id, "executor": r.executor, "status": r.status.as_str(), "started_at": r.started_at, "ended_at": r.ended_at }))
        .collect()
}

pub(super) fn require_state(db: &Db, id: &str, expected: TaskState) -> Result<Task, String> {
    let task = db.get_task(id).map_err(s_err)?;
    if task.state != expected {
        return Err(format!(
            "refused: {id} is {}, not {}",
            task.state.as_str(),
            expected.as_str()
        ));
    }
    Ok(task)
}

pub(super) fn emit_task_event(
    db: &Db,
    id: &str,
    kind: EventKind,
    p: &Value,
    now: i64,
) -> Result<(), String> {
    db.emit(&FactoryEvent::new(kind, p.to_string(), now).for_task(id))
        .map_err(s_err)?;
    Ok(())
}

pub(super) fn s_err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

pub(super) fn resolve_task_tree(db: &Db, task: &Task, ctx: &Ctx) -> PathBuf {
    if let Ok(wt) = worktree_path(Path::new(&task.repo_root), &ctx.worktrees_dir, &task.id)
        && wt.is_dir()
    {
        return wt;
    }
    db.runs_for_task(&task.id)
        .ok()
        .and_then(|rs| {
            rs.into_iter()
                .rev()
                .find(|r| !r.worktree_path.is_empty() && Path::new(&r.worktree_path).is_dir())
        })
        .map_or_else(
            || PathBuf::from(&task.repo_root),
            |r| PathBuf::from(r.worktree_path),
        )
}

pub(super) fn reset_exhausted_budget(db: &Db, root_id: &str) -> Result<bool, PortError> {
    match db.get_budget(root_id) {
        Ok(b) if b.check() != BudgetCheck::Ok => {
            db.put_budget(&RootBudget {
                tokens_used: 0,
                wall_secs_used: 0,
                cycles_used: 0,
                frozen: false,
                ..b
            })?;
            Ok(true)
        }
        Ok(_) | Err(PortError::NotFound(_)) => Ok(false),
        Err(e) => Err(e),
    }
}

pub(super) fn clear_stale_receipts(db: &Db, task: &Task, ctx: &Ctx) {
    let mut roots = vec![PathBuf::from(&task.repo_root)];
    if let Ok(wt) = worktree_path(Path::new(&task.repo_root), &ctx.worktrees_dir, &task.id) {
        roots.push(wt);
    }
    for r in db.runs_for_task(&task.id).unwrap_or_default() {
        if !r.worktree_path.is_empty() {
            roots.push(PathBuf::from(r.worktree_path));
        }
    }
    for root in roots {
        let _ = fs::remove_file(aaf_fp::receipt_path(&ctx.data_dir, &root));
        let _ = fs::remove_dir_all(ctx.data_dir.join("approvals").join(aaf_fp::repo_id(&root)));
    }
}

pub(super) fn maybe_approve_on_unblock(db: &Db, task: &Task, now: i64) -> Result<bool, PortError> {
    let Some(BlockedReason::NeedsHuman { why }) = &task.blocked_reason else {
        return Ok(false);
    };
    let Some(rest) = why.strip_prefix(PLAN_APPROVAL_PREFIX) else {
        return Ok(false);
    };
    let generated = db
        .events_for_task(&task.id, PLAN_EVENT_LOOKBACK)?
        .into_iter()
        .filter(|e| e.kind == EventKind::PlanGenerated)
        .find_map(|e| {
            serde_json::from_str::<Value>(&e.payload_json)
                .ok()
                .and_then(|v| {
                    v["plan"]
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                })
        });
    let plan = generated
        .as_deref()
        .or_else(|| (!rest.trim().is_empty()).then_some(rest.trim()))
        .unwrap_or(why);
    db.append_context_note(&task.id, &format!("{PLAN_APPROVED_PREFIX} {plan}"), now)?;
    Ok(true)
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
