//! Unified human task actions shared by `aaf board`, `aaf ui`, and `aaf telegram`.
//!
//! Centralizing `unblock_task`, `rework_task`, budget reset, plan approval,
//! stale-receipt cleanup, and worktree resolution guarantees all channels keep
//! identical invariants — notably appending an `"unblocked: <source>"` context
//! note on every unblock so the unblocker ladder resets its failure streak.

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

use crate::ctx::Ctx;

pub(crate) const PLAN_APPROVAL_PREFIX: &str = "plan approval required:";
const PLAN_EVENT_LOOKBACK: u32 = 100;

#[derive(Debug)]
pub(crate) enum TaskOpError {
    WrongState {
        task_id: String,
        actual: TaskState,
        expected: TaskState,
    },
    Port(PortError),
}

impl From<PortError> for TaskOpError {
    fn from(err: PortError) -> Self {
        Self::Port(err)
    }
}

impl TaskOpError {
    pub(crate) fn to_refused_string(&self) -> String {
        match self {
            Self::WrongState {
                task_id,
                actual,
                expected,
            } => format!(
                "refused: {task_id} is {}, not {}",
                actual.as_str(),
                expected.as_str()
            ),
            Self::Port(err) => err.to_string(),
        }
    }
}

pub(crate) struct UnblockOutcome {
    pub(crate) budget_reset: bool,
    pub(crate) plan_approved: bool,
}

pub(crate) struct ReworkOutcome {
    pub(crate) target: TaskState,
    pub(crate) budget_reset: bool,
}

/// Parses an optional leading `--rerun` flag from a free-text rework reason.
pub(crate) fn parse_rework_reason(raw: &str) -> (bool, &str) {
    match raw.strip_prefix("--rerun") {
        Some(after) if after.is_empty() || after.starts_with(char::is_whitespace) => {
            (true, after.trim())
        }
        _ => (false, raw.trim()),
    }
}

pub(crate) fn reset_exhausted_budget(db: &Db, root_id: &str) -> Result<bool, PortError> {
    match db.get_budget(root_id) {
        Ok(budget) if budget.check() != BudgetCheck::Ok => {
            db.put_budget(&RootBudget {
                tokens_used: 0,
                wall_secs_used: 0,
                cycles_used: 0,
                frozen: false,
                ..budget
            })?;
            Ok(true)
        }
        Ok(_) | Err(PortError::NotFound(_)) => Ok(false),
        Err(other) => Err(other),
    }
}

pub(crate) fn clear_stale_receipts(db: &impl RunStore, task: &Task, ctx: &Ctx) {
    let main_root = Path::new(&task.repo_root);
    let _ = fs::remove_file(aaf_fp::receipt_path(&ctx.data_dir, main_root));
    let main_id = aaf_fp::repo_id(main_root);
    let main_approvals_dir = ctx.data_dir.join("approvals").join(&main_id);

    let mut wt_roots = Vec::new();
    if let Ok(wt) = worktree_path(main_root, &ctx.worktrees_dir, &task.id) {
        wt_roots.push(wt);
    }
    for r in db.runs_for_task(&task.id).unwrap_or_default() {
        if !r.worktree_path.is_empty() {
            wt_roots.push(PathBuf::from(r.worktree_path));
        }
    }
    for root in wt_roots {
        let _ = fs::remove_file(aaf_fp::receipt_path(&ctx.data_dir, &root));
        let wt_id = aaf_fp::repo_id(&root);
        if wt_id == main_id {
            continue;
        }
        let wt_approvals = ctx.data_dir.join("approvals").join(&wt_id);
        let mut mirrored = Vec::new();
        if let Ok(entries) = fs::read_dir(&wt_approvals) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.to_str().is_some_and(|s| s.ends_with(".json")) {
                    mirrored.push(name);
                }
            }
        }
        let _ = fs::remove_dir_all(&wt_approvals);
        for file_name in mirrored {
            let _ = fs::remove_file(main_approvals_dir.join(file_name));
        }
    }
}

pub(crate) fn latest_generated_plan(db: &Db, task_id: &str) -> Result<Option<String>, PortError> {
    for event in db.events_for_task(task_id, PLAN_EVENT_LOOKBACK)? {
        if event.kind != EventKind::PlanGenerated {
            continue;
        }
        let Ok(payload) = serde_json::from_str::<Value>(&event.payload_json) else {
            continue;
        };
        if let Some(plan) = payload["plan"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Ok(Some(plan.to_string()));
        }
    }
    Ok(None)
}

pub(crate) fn maybe_approve_on_unblock(db: &Db, task: &Task, now: i64) -> Result<bool, PortError> {
    let Some(BlockedReason::NeedsHuman { why }) = &task.blocked_reason else {
        return Ok(false);
    };
    let Some(rest) = why.strip_prefix(PLAN_APPROVAL_PREFIX) else {
        return Ok(false);
    };
    let generated = latest_generated_plan(db, &task.id)?;
    let plan = generated
        .as_deref()
        .or_else(|| (!rest.trim().is_empty()).then_some(rest.trim()))
        .unwrap_or(why);
    db.append_context_note(&task.id, &format!("{PLAN_APPROVED_PREFIX} {plan}"), now)?;
    Ok(true)
}

pub(crate) fn resolve_task_tree(db: &Db, task: &Task, ctx: &Ctx) -> PathBuf {
    if let Ok(wt) = worktree_path(Path::new(&task.repo_root), &ctx.worktrees_dir, &task.id)
        && wt.is_dir()
    {
        return wt;
    }
    db.runs_for_task(&task.id)
        .ok()
        .and_then(|runs| {
            runs.into_iter()
                .rev()
                .map(|r| PathBuf::from(r.worktree_path))
                .find(|p| !p.as_os_str().is_empty() && p.is_dir())
        })
        .unwrap_or_else(|| PathBuf::from(&task.repo_root))
}

/// Moves a `Blocked` task back to `Ready`, resets any exhausted root budget,
/// approves a pending generated plan if applicable, appends an `"unblocked: <why>"`
/// context note, and records `TaskStateChanged`.
pub(crate) fn unblock_task(
    db: &Db,
    task_id: &str,
    why: &str,
    by: &str,
    now: i64,
) -> Result<UnblockOutcome, TaskOpError> {
    let task = db.get_task(task_id)?;
    if task.state != TaskState::Blocked {
        return Err(TaskOpError::WrongState {
            task_id: task_id.to_string(),
            actual: task.state,
            expected: TaskState::Blocked,
        });
    }
    let budget_reset = reset_exhausted_budget(db, &task.root_id)?;
    let plan_approved = maybe_approve_on_unblock(db, &task, now)?;
    db.append_context_note(task_id, &format!("unblocked: {why}"), now)?;
    db.set_state(task_id, TaskState::Ready, None, now)?;
    let payload = json!({
        "to": "ready",
        "why": why,
        "by": by,
        "was": task.blocked_reason,
        "budget_reset": budget_reset,
        "plan_approved": plan_approved,
    });
    db.emit(
        &FactoryEvent::new(EventKind::TaskStateChanged, payload.to_string(), now).for_task(task_id),
    )?;
    Ok(UnblockOutcome {
        budget_reset,
        plan_approved,
    })
}

/// Moves a `Review` task back to `Gating` (or `Ready` when `rerun == true`),
/// resets any exhausted root budget, clears stale receipts/approvals, appends
/// a `"rework: ..."` note, and emits `ReworkRequested` + `TaskStateChanged`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn rework_task(
    db: &Db,
    ctx: &Ctx,
    task_id: &str,
    reason: &str,
    rerun: bool,
    why: &str,
    by: &str,
    now: i64,
) -> Result<ReworkOutcome, TaskOpError> {
    let task = db.get_task(task_id)?;
    if task.state != TaskState::Review {
        return Err(TaskOpError::WrongState {
            task_id: task_id.to_string(),
            actual: task.state,
            expected: TaskState::Review,
        });
    }
    let target = if rerun {
        TaskState::Ready
    } else {
        TaskState::Gating
    };
    let budget_reset = reset_exhausted_budget(db, &task.root_id)?;
    clear_stale_receipts(db, &task, ctx);
    let note = format!(
        "rework: {}",
        reason.split_whitespace().collect::<Vec<_>>().join(" ")
    );
    db.append_context_note(task_id, &note, now)?;
    db.set_state(task_id, target, None, now)?;
    let rework_payload = json!({
        "reason": reason,
        "to": target.as_str(),
        "rerun": rerun,
        "budget_reset": budget_reset,
        "by": by,
    });
    db.emit(
        &FactoryEvent::new(EventKind::ReworkRequested, rework_payload.to_string(), now)
            .for_task(task_id),
    )?;
    let move_payload = json!({
        "from": TaskState::Review.as_str(),
        "to": target.as_str(),
        "why": why,
        "by": by,
    });
    db.emit(
        &FactoryEvent::new(EventKind::TaskStateChanged, move_payload.to_string(), now)
            .for_task(task_id),
    )?;
    Ok(ReworkOutcome {
        target,
        budget_reset,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use aaf_domain::task::{ImmutableSpec, Priority};

    #[test]
    fn unblock_task_appends_unblocked_note_and_transitions_to_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
        let mut task = Task::new_root(
            "t-blk",
            "/repo",
            "Blocked task",
            ImmutableSpec {
                description: "desc".into(),
                acceptance: vec!["ok".into()],
            },
            Priority::P2,
            10,
        );
        task.state = TaskState::Blocked;
        task.blocked_reason = Some(BlockedReason::Stuck {
            detail: "two fails".into(),
        });
        db.insert_task(&task).unwrap();

        let out = unblock_task(&db, "t-blk", "aaf board unblock", "human", 20).unwrap();
        assert!(!out.budget_reset);
        assert!(!out.plan_approved);

        let updated = db.get_task("t-blk").unwrap();
        assert_eq!(updated.state, TaskState::Ready);
        assert!(
            updated
                .context_notes
                .iter()
                .any(|n| n == "unblocked: aaf board unblock"),
            "missing unblocked note: {:?}",
            updated.context_notes
        );
    }

    #[test]
    fn clear_stale_receipts_preserves_sibling_main_approvals() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let data = tmp.path().join("data");
        let worktrees = tmp.path().join("worktrees");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&data).unwrap();
        let db = Db::open(&data.join("aaf.db")).unwrap();
        let task = Task::new_root(
            "t1",
            repo.to_string_lossy().into_owned(),
            "Task 1",
            ImmutableSpec {
                description: "desc".into(),
                acceptance: vec![],
            },
            Priority::P2,
            1,
        );
        let wt = worktree_path(&repo, &worktrees, &task.id).unwrap();
        fs::create_dir_all(&wt).unwrap();
        let main_dir = data.join("approvals").join(aaf_fp::repo_id(&repo));
        let wt_dir = data.join("approvals").join(aaf_fp::repo_id(&wt));
        fs::create_dir_all(&main_dir).unwrap();
        fs::create_dir_all(&wt_dir).unwrap();
        fs::write(wt_dir.join("fp-task.json"), "{}").unwrap();
        fs::write(main_dir.join("fp-task.json"), "{}").unwrap();
        fs::write(main_dir.join("fp-sibling.json"), "{}").unwrap();

        let ctx = Ctx {
            cwd: repo.clone(),
            home: tmp.path().join("home"),
            data_dir: data,
            config_dir: tmp.path().join("config"),
            worktrees_dir: worktrees,
            path_var: std::ffi::OsString::new(),
            run_id: None,
            stdin_is_tty: false,
            aaf_env: Vec::new(),
        };
        clear_stale_receipts(&db, &task, &ctx);

        assert!(!wt_dir.exists());
        assert!(!main_dir.join("fp-task.json").exists());
        assert!(main_dir.join("fp-sibling.json").exists());
    }
}
