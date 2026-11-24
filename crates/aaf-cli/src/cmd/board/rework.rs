//! `aaf board rework <id> --reason "..." [--rerun]`: send a task in `review`
//! back to `gating` (re-gate existing branch) or `ready` (`--rerun`).

use aaf_domain::ports::{Clock, TaskStore};
use aaf_engine::factory::adapters::SystemClock;
use serde_json::json;

use super::store::{open_db, refuse_inside_agent_run};
use super::{clean, port, task_json};
use crate::cmd::task_ops::{TaskOpError, rework_task};
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

pub fn run(args: &ReworkArgs, ctx: &Ctx) -> CmdResult {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf board rework") {
        return Ok(refused);
    }
    let reason = clean("--reason", &args.reason)?;
    let db = open_db(ctx)?;
    let now = SystemClock.now();
    let out = match rework_task(
        &db,
        ctx,
        &args.id,
        &reason,
        args.rerun,
        "aaf board rework",
        "human",
        now,
    ) {
        Ok(out) => out,
        Err(TaskOpError::WrongState { actual, .. }) => {
            let text = format!("{} is {}, not review", args.id, actual.as_str());
            return Ok(CmdOutput::policy("refused", text).field("state", json!(actual)));
        }
        Err(TaskOpError::Port(other)) => return Err(port(other)),
    };
    let updated = db.get_task(&args.id).map_err(port)?;
    Ok(CmdOutput::ok(
        "ok",
        format!("{} sent back to {}", args.id, out.target.as_str()),
    )
    .field("task_id", json!(args.id))
    .field("state", json!(out.target))
    .field("budget_reset", json!(out.budget_reset))
    .field("task", task_json(&updated)?))
}
