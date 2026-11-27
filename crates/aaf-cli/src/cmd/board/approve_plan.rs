//! `aaf board approve-plan [<id> | --all] [--run]`: approve generated
//! implementation plan(s) and move plan-gated task(s) from `blocked` back to
//! `ready`, optionally kicking off `aaf run`.

use aaf_domain::blocked::BlockedReason;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{Clock, EventSink, TaskStore};
use aaf_domain::task::{Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::factory::adapters::SystemClock;
use clap::Parser;
use serde_json::{Value, json};

use super::port;
use super::store::{open_db, refuse_inside_agent_run, repo_root};
use crate::cmd::task_ops::{
    LatestPlan, PLAN_APPROVAL_PREFIX, PLAN_DEBATE_STALLED_PREFIX, emit_human_plan_converged,
    format_approved_plan_note, latest_plan_info, reset_exhausted_budget,
};
use crate::ctx::Ctx;
use crate::output::{CmdError, CmdOutput, CmdResult, Exit};

#[derive(Debug, clap::Args)]
pub struct ApprovePlanArgs {
    /// Task id waiting for plan approval (required unless `--all` is set).
    #[arg(required_unless_present = "all", conflicts_with = "all")]
    pub id: Option<String>,
    /// Approve every plan-gated Blocked task in the current repository.
    #[arg(long)]
    pub all: bool,
    /// Immediately run `aaf run` after approving the plan(s).
    #[arg(long)]
    pub run: bool,
}

#[derive(Debug, Parser)]
struct DefaultRunParser {
    #[command(flatten)]
    args: crate::cmd::run::RunArgs,
}

pub fn run_args(ctx: &Ctx, args: &ApprovePlanArgs) -> CmdResult {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf board approve-plan") {
        return Ok(refused);
    }
    if let Some(task_id) = args.id.as_deref() {
        let out = run(ctx, task_id)?;
        if out.exit != Exit::Ok || !args.run {
            return Ok(out);
        }
        return run_after_approve(ctx, out);
    }

    let repo = repo_root(ctx)?.to_string_lossy().into_owned();
    let db = open_db(ctx)?;
    let blocked = db
        .list_tasks(Some(&repo), Some(TaskState::Blocked))
        .map_err(port)?;
    let now = SystemClock.now();
    let mut approved_ids = Vec::new();
    let mut skipped = Vec::new();

    for task in blocked {
        if !is_awaiting_explicit_plan_approval(task.blocked_reason.as_ref()) {
            continue;
        }
        let Some(plan) = latest_plan_info(&db, &task.id).map_err(port)? else {
            skipped.push(task.id);
            continue;
        };
        apply_plan_approval(&db, &task, &plan, now)?;
        approved_ids.push(task.id);
    }

    let text = if approved_ids.is_empty() {
        "no tasks waiting for plan approval".to_string()
    } else {
        format!(
            "approved plan for {} task(s): {}",
            approved_ids.len(),
            approved_ids.join(", ")
        )
    };
    let out = CmdOutput::ok("ok", text)
        .field("all", json!(true))
        .field("count", json!(approved_ids.len()))
        .field("approved_ids", json!(approved_ids))
        .field("skipped", json!(skipped))
        .field("approved", json!(!approved_ids.is_empty()));

    if args.run && !approved_ids.is_empty() {
        return run_after_approve(ctx, out);
    }
    Ok(out.field("ran", json!(false)))
}

pub fn run(ctx: &Ctx, task_id: &str) -> Result<CmdOutput, CmdError> {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf board approve-plan") {
        return Ok(refused);
    }
    let db = open_db(ctx)?;
    let task = db.get_task(task_id).map_err(port)?;
    if task.state != TaskState::Blocked {
        let text = format!("{task_id} is {}, not blocked", task.state.as_str());
        return Ok(CmdOutput::policy("refused", text).field("state", json!(task.state)));
    }
    if !is_waiting_for_plan_approval(task.blocked_reason.as_ref()) {
        let text = format!("{task_id} is not waiting for plan approval");
        return Ok(CmdOutput::policy("refused", text).field("state", json!(task.state)));
    }
    let Some(plan) = latest_plan_info(&db, task_id).map_err(port)? else {
        let text = format!("{task_id} has no generated plan event to approve");
        return Ok(CmdOutput::policy("refused", text).field("task_id", json!(task_id)));
    };
    let now = SystemClock.now();
    apply_plan_approval(&db, &task, &plan, now)?;
    Ok(
        CmdOutput::ok("ok", format!("{task_id} plan approved; task is ready"))
            .field("task_id", json!(task_id))
            .field("state", json!("ready"))
            .field("approved", json!(true)),
    )
}

fn is_awaiting_explicit_plan_approval(reason: Option<&BlockedReason>) -> bool {
    matches!(
        reason,
        Some(BlockedReason::NeedsHuman { why }) if why.starts_with(PLAN_APPROVAL_PREFIX)
    )
}

fn is_waiting_for_plan_approval(reason: Option<&BlockedReason>) -> bool {
    is_awaiting_explicit_plan_approval(reason)
        || matches!(
            reason,
            Some(BlockedReason::NeedsHuman { why }) if why.starts_with(PLAN_DEBATE_STALLED_PREFIX)
        )
}

fn apply_plan_approval(db: &Db, task: &Task, plan: &LatestPlan, now: i64) -> Result<(), CmdError> {
    let note = format_approved_plan_note(plan);
    db.append_context_note(&task.id, &note, now).map_err(port)?;
    emit_human_plan_converged(db, &task.id, plan, now).map_err(port)?;
    let budget_reset = reset_exhausted_budget(db, &task.root_id, &task.repo_root).map_err(port)?;
    db.set_state(&task.id, TaskState::Ready, None, now)
        .map_err(port)?;
    let payload = json!({
        "from": TaskState::Blocked.as_str(),
        "to": TaskState::Ready.as_str(),
        "why": "plan_approved",
        "by": "human",
        "budget_reset": budget_reset,
    });
    let event = FactoryEvent::new(EventKind::TaskStateChanged, payload.to_string(), now);
    db.emit(&event.for_task(&task.id)).map_err(port)?;
    Ok(())
}

fn run_after_approve(ctx: &Ctx, approve_out: CmdOutput) -> CmdResult {
    let parsed = DefaultRunParser::try_parse_from(["run"])
        .map_err(|e| CmdError::new(format!("default run args: {e}")))?;
    let run_out = crate::cmd::run::run(&parsed.args, ctx)?;
    let run_status = run_out.status;
    let combined_text = format!("{}\n{}", approve_out.text, run_out.text);
    let mut out = if run_out.exit == Exit::Ok {
        CmdOutput::ok(run_status.clone(), combined_text)
    } else {
        CmdOutput::policy(run_status.clone(), combined_text)
    };
    out.fields = approve_out.fields;
    Ok(out
        .field("ran", json!(true))
        .field("run_status", json!(run_status))
        .field("run", Value::Object(run_out.fields)))
}
