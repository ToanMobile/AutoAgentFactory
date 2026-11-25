//! `aaf board cancel <id> [--reason <text>] [--cascade]`: atomically cancels a
//! non-active task (and optionally its unfinished descendants) while surfacing
//! split-parent and DAG dependent warnings.

use aaf_domain::ports::Clock;
use aaf_domain::task::TaskState;
use aaf_engine::db::CancelRefusal;
use aaf_engine::factory::adapters::SystemClock;
use serde_json::json;

use super::store::{open_db, refuse_inside_agent_run};
use super::{clean, port};
use crate::ctx::Ctx;
use crate::output::{CmdOutput, CmdResult};

#[derive(Debug, clap::Args)]
pub struct CancelArgs {
    /// Task id to cancel.
    pub id: String,
    /// Optional reason recorded on the TaskStateChanged event.
    #[arg(long)]
    pub reason: Option<String>,
    /// Also cancel unfinished child/descendant tasks atomically.
    #[arg(long)]
    pub cascade: bool,
    /// Allow cancelling an idle Gating task (e.g. imported from GitHub QA) when no run is active.
    #[arg(long)]
    pub force: bool,
}

pub fn run(args: &CancelArgs, ctx: &Ctx) -> CmdResult {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf board cancel") {
        return Ok(refused);
    }
    let reason = args
        .reason
        .as_deref()
        .map(|raw| clean("--reason", raw))
        .transpose()?;
    let db = open_db(ctx)?;
    let now = SystemClock.now();

    let outcome = match db
        .cancel_task_atomic_with_options(&args.id, args.cascade, args.force, reason.as_deref(), now)
        .map_err(port)?
    {
        Ok(outcome) => outcome,
        Err(refusal) => return Ok(format_refusal(&args.id, refusal)),
    };

    let mut warnings = Vec::new();
    if let Some(parent_id) = &outcome.parent_id {
        warnings.push(format!(
            "{} is a subtask of {parent_id}: split parent {parent_id} will not auto-complete \
             because try_auto_complete_split_parent requires every child to be done",
            outcome.task_id
        ));
    }
    if !outcome.affected_dependents.is_empty() {
        warnings.push(format!(
            "cancelled task(s) still block dependent task(s): {} \
             (use `aaf board dep rm <task> --blocked-by <id>` or cancel them)",
            outcome.affected_dependents.join(", ")
        ));
    }

    let mut lines = Vec::with_capacity(1 + warnings.len());
    if outcome.cancelled_ids.len() == 1 {
        lines.push(format!("cancelled {}", outcome.task_id));
    } else {
        lines.push(format!(
            "cancelled {} task(s): {}",
            outcome.cancelled_ids.len(),
            outcome.cancelled_ids.join(", ")
        ));
    }
    for warning in &warnings {
        lines.push(format!("warning: {warning}"));
    }

    let mut out = CmdOutput::ok("cancelled", lines.join("\n"))
        .field("task_id", json!(outcome.task_id))
        .field("state", json!(TaskState::Cancelled))
        .field("cancelled_ids", json!(outcome.cancelled_ids))
        .field("affected_dependents", json!(outcome.affected_dependents))
        .field("warnings", json!(warnings));
    if let Some(reason) = reason {
        out = out.field("reason", json!(reason));
    }
    Ok(out)
}

fn format_refusal(target_id: &str, refusal: CancelRefusal) -> CmdOutput {
    match refusal {
        CancelRefusal::ActiveState { task_id, state } => {
            let hint = if state == TaskState::Gating {
                "stop `aaf run` first, or pass --force if no run is active"
            } else {
                "stop `aaf run` first"
            };
            let text = format!(
                "{task_id} is {}; cannot cancel an active task ({hint})",
                state.as_str()
            );
            CmdOutput::policy("refused", text)
                .field("task_id", json!(task_id))
                .field("state", json!(state))
        }
        CancelRefusal::AlreadyTerminal { task_id, state } => {
            let text = format!("{task_id} is already {}", state.as_str());
            CmdOutput::policy("refused", text)
                .field("task_id", json!(task_id))
                .field("state", json!(state))
        }
        CancelRefusal::RunningRun { task_id, run_id } => {
            let text = format!(
                "{task_id} has a running run ({run_id}); cannot cancel while a run is active"
            );
            CmdOutput::policy("refused", text)
                .field("task_id", json!(task_id))
                .field("run_id", json!(run_id))
        }
        CancelRefusal::ActiveDescendant { child_id, state } => {
            let text = format!(
                "cannot cancel {target_id}: subtask {child_id} is {}",
                state.as_str()
            );
            CmdOutput::policy("refused", text)
                .field("task_id", json!(target_id))
                .field("child_id", json!(child_id))
                .field("state", json!(state))
        }
        CancelRefusal::RunningDescendantRun { child_id, run_id } => {
            let text = format!(
                "cannot cancel {target_id}: subtask {child_id} has a running run ({run_id})"
            );
            CmdOutput::policy("refused", text)
                .field("task_id", json!(target_id))
                .field("child_id", json!(child_id))
                .field("run_id", json!(run_id))
        }
        CancelRefusal::HasUnfinishedChildren { children } => {
            let text = format!(
                "{target_id} has {} unfinished subtask(s) ({}); pass --cascade to cancel them together",
                children.len(),
                children.join(", ")
            );
            CmdOutput::policy("refused", text)
                .field("task_id", json!(target_id))
                .field("children", json!(children))
                .field("requires_cascade", json!(true))
        }
    }
}
