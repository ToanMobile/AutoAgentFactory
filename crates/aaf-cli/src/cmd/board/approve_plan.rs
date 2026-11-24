//! `aaf board approve-plan <id>`: approve a generated implementation plan and
//! move a plan-gated task from `blocked` back to `ready`.

use aaf_domain::blocked::BlockedReason;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{Clock, EventSink, TaskStore};
use aaf_domain::task::TaskState;
use aaf_engine::factory::adapters::SystemClock;
use aaf_engine::factory::prompt::PLAN_APPROVED_PREFIX;
use serde_json::json;

use super::port;
use super::store::{open_db, refuse_inside_agent_run};
use crate::cmd::task_ops::{PLAN_APPROVAL_PREFIX, latest_generated_plan};
use crate::ctx::Ctx;
use crate::output::{CmdError, CmdOutput};

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
    let waiting_for_plan = matches!(
        &task.blocked_reason,
        Some(BlockedReason::NeedsHuman { why }) if why.starts_with(PLAN_APPROVAL_PREFIX)
    );
    if !waiting_for_plan {
        let text = format!("{task_id} is not waiting for plan approval");
        return Ok(CmdOutput::policy("refused", text).field("state", json!(task.state)));
    }
    let Some(plan) = latest_generated_plan(&db, task_id).map_err(port)? else {
        let text = format!("{task_id} has no generated plan event to approve");
        return Ok(CmdOutput::policy("refused", text).field("task_id", json!(task_id)));
    };
    let now = SystemClock.now();
    let note = format!("{PLAN_APPROVED_PREFIX} {plan}");
    db.append_context_note(task_id, &note, now).map_err(port)?;
    db.set_state(task_id, TaskState::Ready, None, now)
        .map_err(port)?;
    let payload = json!({
        "from": TaskState::Blocked.as_str(),
        "to": TaskState::Ready.as_str(),
        "why": "plan_approved",
        "by": "human",
    });
    let event = FactoryEvent::new(EventKind::TaskStateChanged, payload.to_string(), now);
    db.emit(&event.for_task(task_id)).map_err(port)?;
    Ok(
        CmdOutput::ok("ok", format!("{task_id} plan approved; task is ready"))
            .field("task_id", json!(task_id))
            .field("state", json!("ready"))
            .field("approved", json!(true)),
    )
}
