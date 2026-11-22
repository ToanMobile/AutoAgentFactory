//! `aaf board approve-plan <id>`: approve a generated implementation plan and
//! move a plan-gated task from `blocked` back to `ready`.

use aaf_domain::blocked::BlockedReason;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{Clock, EventSink, TaskStore};
use aaf_domain::task::{Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::factory::adapters::SystemClock;
use aaf_engine::factory::prompt::PLAN_APPROVED_PREFIX;
use serde_json::{Value, json};

use super::port;
use super::store::{open_db, refuse_inside_agent_run};
use crate::ctx::Ctx;
use crate::output::{CmdError, CmdOutput};

const PLAN_APPROVAL_PREFIX: &str = "plan approval required:";
const PLAN_EVENT_LOOKBACK: u32 = 100;

fn latest_generated_plan(db: &Db, task_id: &str) -> Result<Option<String>, CmdError> {
    let events = db
        .events_for_task(task_id, PLAN_EVENT_LOOKBACK)
        .map_err(port)?;
    for event in events {
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
    let Some(plan) = latest_generated_plan(&db, task_id)? else {
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

/// When a human runs `aaf board unblock` on a task waiting for plan approval,
/// also record `plan_approved:` so the next turn proceeds to implementation
/// instead of re-entering the plan stage.
pub fn maybe_approve_on_unblock(db: &Db, task: &Task, now: i64) -> Result<bool, CmdError> {
    let Some(BlockedReason::NeedsHuman { why }) = &task.blocked_reason else {
        return Ok(false);
    };
    if !why.starts_with(PLAN_APPROVAL_PREFIX) {
        return Ok(false);
    }
    let plan = match latest_generated_plan(db, &task.id)? {
        Some(plan) => plan,
        None => why
            .strip_prefix(PLAN_APPROVAL_PREFIX)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(why.as_str())
            .to_string(),
    };
    let note = format!("{PLAN_APPROVED_PREFIX} {plan}");
    db.append_context_note(&task.id, &note, now).map_err(port)?;
    Ok(true)
}
