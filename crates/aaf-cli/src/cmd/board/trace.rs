//! `aaf board trace <id>`: chronological timeline of runs, events, tokens, and cost.

use aaf_domain::ports::Clock;
use aaf_engine::factory::adapters::SystemClock;
use aaf_engine::telemetry::{CostRates, collect_trace_at};
use serde_json::json;

use super::store::{err, open_db_read_only};
use crate::ctx::Ctx;
use crate::output::{CmdError, CmdOutput, CmdResult};

#[derive(Debug, clap::Args)]
pub struct TraceArgs {
    pub id: String,
    #[arg(long, default_value_t = 100)]
    pub limit: u32,
}

pub fn run(args: &TraceArgs, ctx: &Ctx) -> CmdResult {
    let Some(db) = open_db_read_only(ctx)? else {
        return Err(CmdError::new(format!("not found: task {}", args.id)));
    };
    let trace = collect_trace_at(
        &db,
        &db,
        &db,
        &args.id,
        CostRates::default(),
        args.limit,
        SystemClock.now(),
    )
    .map_err(|e| CmdError::new(e.to_string()))?;

    let mut lines = vec![format!(
        "{}  {}  {}  tokens={}+{}  wall={}s  cost=${:.4}",
        trace.task_id,
        trace.state.as_str(),
        trace.title,
        trace.total_tokens_in,
        trace.total_tokens_out,
        trace.total_wall_secs,
        trace.estimated_cost_usd
    )];
    for entry in &trace.entries {
        lines.push(format!(
            "  {} [{}] {}",
            entry.timestamp, entry.source, entry.summary
        ));
    }
    if trace.events_truncated {
        lines.push(format!(
            "  ... (events truncated at limit={}; pass --limit to show more)",
            args.limit
        ));
    }

    let json_trace = serde_json::to_value(&trace).map_err(|e| err("trace", e))?;
    Ok(CmdOutput::ok("ok", lines.join("\n"))
        .field("task_id", json!(trace.task_id))
        .field("events_truncated", json!(trace.events_truncated))
        .field("trace", json_trace))
}
