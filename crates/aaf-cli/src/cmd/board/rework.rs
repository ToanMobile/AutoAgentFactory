//! `aaf board rework <id> --reason "..." [--rerun]`: send a task in `review`
//! back to `gating` (re-gate existing branch) or `ready` (`--rerun`).

use std::fs;
use std::path::{Path, PathBuf};

use aaf_domain::budget::{BudgetCheck, RootBudget};
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{BudgetStore, Clock, EventSink, PortError, RunStore, TaskStore};
use aaf_domain::task::{Task, TaskState};
use aaf_engine::factory::adapters::SystemClock;
use serde_json::json;

use super::store::{open_db, refuse_inside_agent_run};
use super::{clean, port, task_json};
use crate::ctx::Ctx;
use crate::output::{CmdOutput, CmdResult};

#[derive(Debug, clap::Args)]
pub struct ReworkArgs {
    pub id: String,
    /// Why this review item is sent back for rework.
    #[arg(long, alias = "note")]
    pub reason: String,
    /// Move back to `ready` so the next `aaf run` invokes an agent instead of re-gating.
    #[arg(long)]
    pub rerun: bool,
}

fn clear_stale_receipts(db: &impl RunStore, task: &Task, ctx: &Ctx) {
    let mut roots = vec![PathBuf::from(&task.repo_root)];
    if let Ok(wt) = aaf_engine::git::worktree::worktree_path(
        Path::new(&task.repo_root),
        &ctx.worktrees_dir,
        &task.id,
    ) {
        roots.push(wt);
    }
    if let Ok(runs) = db.runs_for_task(&task.id) {
        for run in runs {
            if !run.worktree_path.is_empty() {
                roots.push(PathBuf::from(run.worktree_path));
            }
        }
    }
    for root in roots {
        let path = aaf_fp::receipt_path(&ctx.data_dir, &root);
        let _ = fs::remove_file(path);
        let approvals_dir = ctx.data_dir.join("approvals").join(aaf_fp::repo_id(&root));
        let _ = fs::remove_dir_all(approvals_dir);
    }
}

pub fn run(args: &ReworkArgs, ctx: &Ctx) -> CmdResult {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf board rework") {
        return Ok(refused);
    }
    let reason = clean("--reason", &args.reason)?;
    let db = open_db(ctx)?;
    let task = db.get_task(&args.id).map_err(port)?;
    if task.state != TaskState::Review {
        let text = format!("{} is {}, not review", args.id, task.state.as_str());
        return Ok(CmdOutput::policy("refused", text).field("state", json!(task.state)));
    }
    let target = if args.rerun {
        TaskState::Ready
    } else {
        TaskState::Gating
    };
    let now = SystemClock.now();
    // Mirror `aaf board unblock`: reset an exhausted/frozen root budget so
    // the reworked task can actually execute instead of freezing immediately.
    let budget_reset = match db.get_budget(&task.root_id) {
        Ok(budget) if budget.check() != BudgetCheck::Ok => {
            db.put_budget(&RootBudget {
                tokens_used: 0,
                wall_secs_used: 0,
                cycles_used: 0,
                frozen: false,
                ..budget
            })
            .map_err(port)?;
            true
        }
        Ok(_) | Err(PortError::NotFound(_)) => false,
        Err(other) => return Err(port(other)),
    };
    // Drop any signed pre-review receipt so Review -> Gating re-runs the gate.
    clear_stale_receipts(&db, &task, ctx);
    let note = format!(
        "rework: {}",
        reason.split_whitespace().collect::<Vec<_>>().join(" ")
    );
    db.append_context_note(&args.id, &note, now).map_err(port)?;
    db.set_state(&args.id, target, None, now).map_err(port)?;

    let rework_payload = json!({
        "reason": reason,
        "to": target.as_str(),
        "rerun": args.rerun,
        "budget_reset": budget_reset,
        "by": "human",
    });
    db.emit(
        &FactoryEvent::new(EventKind::ReworkRequested, rework_payload.to_string(), now)
            .for_task(&args.id),
    )
    .map_err(port)?;
    let move_payload = json!({
        "from": TaskState::Review.as_str(),
        "to": target.as_str(),
        "why": "aaf board rework",
        "by": "human",
    });
    db.emit(
        &FactoryEvent::new(EventKind::TaskStateChanged, move_payload.to_string(), now)
            .for_task(&args.id),
    )
    .map_err(port)?;
    let updated = db.get_task(&args.id).map_err(port)?;
    Ok(CmdOutput::ok(
        "ok",
        format!("{} sent back to {}", args.id, target.as_str()),
    )
    .field("task_id", json!(args.id))
    .field("state", json!(target))
    .field("budget_reset", json!(budget_reset))
    .field("task", task_json(&updated)?))
}
