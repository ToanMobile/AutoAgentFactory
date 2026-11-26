//! `aaf board trace <id>`: chronological timeline of runs, events, tokens, cost,
//! and optional zero-LLM stream-log forensics (`--forensics`).

use std::collections::HashMap;
use std::path::Path;

use aaf_domain::ports::Clock;
use aaf_engine::executors::stream_log::{read_tail_lines, stream_exists};
use aaf_engine::factory::adapters::SystemClock;
use aaf_engine::telemetry::collect_trace_at;
use serde::Serialize;
use serde_json::{Value, json};

use super::store::{err, open_db_read_only};
use crate::ctx::Ctx;
use crate::output::{CmdError, CmdOutput, CmdResult};

const FORENSICS_MAX_LINES: usize = 50_000;
const TOP_REPEATED_TOOLS: usize = 10;

#[derive(Debug, clap::Args)]
pub struct TraceArgs {
    pub id: String,
    #[arg(long, default_value_t = 100)]
    pub limit: u32,
    /// Inspect `<data_dir>/streams/<run_id>.jsonl` read-only for peak context,
    /// repeated tool calls, nested subagent dispatches, errors, and rate limits.
    #[arg(long)]
    pub forensics: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepeatedTool {
    pub tool: String,
    pub target: String,
    pub count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunForensics {
    pub run_id: String,
    pub tool_calls: usize,
    pub nested_subagents: usize,
    pub error_events: usize,
    pub rate_limit_events: usize,
    pub truncated: bool,
    pub peak_context_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub ctx_pct: Option<u8>,
    pub model: Option<String>,
    pub repeated_tools: Vec<RepeatedTool>,
}

pub fn run(args: &TraceArgs, ctx: &Ctx) -> CmdResult {
    let Some(db) = open_db_read_only(ctx)? else {
        return Err(CmdError::new(format!("not found: task {}", args.id)));
    };
    let cost = ctx.cost_policy(&db);
    let trace = collect_trace_at(
        &db,
        &db,
        &db,
        &args.id,
        &cost,
        args.limit,
        SystemClock.now(),
    )
    .map_err(|e| CmdError::new(e.to_string()))?;

    let cost_str = match trace.estimated_cost_usd {
        Some(usd) => format!("${usd:.4}"),
        None => "n/a".to_string(),
    };
    let mut lines = vec![format!(
        "{}  {}  {}  tokens={}+{}  wall={}s  cost={}",
        trace.task_id,
        trace.state.as_str(),
        trace.title,
        trace.total_tokens_in,
        trace.total_tokens_out,
        trace.total_wall_secs,
        cost_str
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

    let forensics = if args.forensics {
        let run_ids: Vec<&str> = trace
            .entries
            .iter()
            .filter(|e| e.source == "run")
            .filter_map(|e| e.run_id.as_deref())
            .collect();
        let report = collect_task_forensics(&ctx.data_dir, &run_ids);
        if !report.is_empty() {
            lines.push("Forensics (stream logs):".to_string());
            for item in &report {
                lines.push(format_forensics_line(item));
                for rep in &item.repeated_tools {
                    lines.push(format!(
                        "    repeat {}x {}: {}",
                        rep.count, rep.tool, rep.target
                    ));
                }
            }
        } else {
            lines.push("Forensics: no stream logs recorded for runs of this task.".to_string());
        }
        Some(report)
    } else {
        None
    };

    let json_trace = serde_json::to_value(&trace).map_err(|e| err("trace", e))?;
    let mut out = CmdOutput::ok("ok", lines.join("\n"))
        .field("task_id", json!(trace.task_id))
        .field("events_truncated", json!(trace.events_truncated))
        .field("trace", json_trace);
    if let Some(report) = forensics {
        let val = serde_json::to_value(&report).map_err(|e| err("forensics", e))?;
        out = out.field("forensics", val);
    }
    Ok(out)
}

fn collect_task_forensics(data_dir: &Path, run_ids: &[&str]) -> Vec<RunForensics> {
    let mut out = Vec::new();
    for &run_id in run_ids {
        if !stream_exists(data_dir, run_id) {
            continue;
        }
        if let Ok(lines) = read_tail_lines(data_dir, run_id, FORENSICS_MAX_LINES) {
            out.push(analyze_stream_lines(run_id, &lines));
        }
    }
    out
}

fn format_forensics_line(f: &RunForensics) -> String {
    let ctx_part = match (f.peak_context_tokens, f.ctx_pct) {
        (Some(peak), Some(pct)) => format!("peak_ctx={peak} ({pct}%)"),
        (Some(peak), None) => format!("peak_ctx={peak}"),
        _ => "peak_ctx=n/a".to_string(),
    };
    let trunc = if f.truncated { " [truncated]" } else { "" };
    format!(
        "  run {}  {}  tools={}  subagents={}  errors={}  rate_limits={}{}",
        f.run_id,
        ctx_part,
        f.tool_calls,
        f.nested_subagents,
        f.error_events,
        f.rate_limit_events,
        trunc
    )
}

fn repeat_threshold(tool: &str) -> usize {
    match tool {
        "Read" | "Grep" | "Glob" => 3,
        _ => 2,
    }
}

fn parse_finish_kv(target: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    target
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix(&prefix))
        .map(str::to_string)
}

pub(crate) fn analyze_stream_lines(run_id: &str, lines: &[String]) -> RunForensics {
    let mut tool_calls = 0usize;
    let mut nested_subagents = 0usize;
    let mut error_events = 0usize;
    let mut rate_limit_events = 0usize;
    let mut truncated = false;
    let mut peak_context_tokens = None;
    let mut cache_read_tokens = None;
    let mut ctx_pct = None;
    let mut model = None;
    let mut counts: HashMap<(String, String), usize> = HashMap::new();

    for raw in lines {
        let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(raw) else {
            continue;
        };
        match obj.get("type").and_then(Value::as_str) {
            Some("tool_use") => {
                tool_calls = tool_calls.saturating_add(1);
                let tool = obj.get("tool").and_then(Value::as_str).unwrap_or("unknown");
                if matches!(tool, "Agent" | "Task") {
                    nested_subagents = nested_subagents.saturating_add(1);
                }
                if let Some(target) = obj
                    .get("target")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    let key = (tool.to_string(), target.to_string());
                    *counts.entry(key).or_insert(0) += 1;
                }
            }
            Some("error") => error_events = error_events.saturating_add(1),
            Some("rate_limit") => rate_limit_events = rate_limit_events.saturating_add(1),
            Some("truncated") => truncated = true,
            Some("finish") => {
                if let Some(target) = obj.get("target").and_then(Value::as_str) {
                    peak_context_tokens =
                        parse_finish_kv(target, "peak_ctx").and_then(|v| v.parse().ok());
                    cache_read_tokens =
                        parse_finish_kv(target, "cache_read").and_then(|v| v.parse().ok());
                    ctx_pct = parse_finish_kv(target, "ctx_pct")
                        .and_then(|v| v.trim_end_matches('%').parse().ok());
                    model = parse_finish_kv(target, "model");
                }
            }
            _ => {}
        }
    }

    let mut repeated_tools: Vec<RepeatedTool> = counts
        .into_iter()
        .filter(|((tool, _), count)| *count >= repeat_threshold(tool))
        .map(|((tool, target), count)| RepeatedTool {
            tool,
            target,
            count,
        })
        .collect();
    repeated_tools.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.tool.cmp(&b.tool))
            .then_with(|| a.target.cmp(&b.target))
    });
    repeated_tools.truncate(TOP_REPEATED_TOOLS);

    RunForensics {
        run_id: run_id.to_string(),
        tool_calls,
        nested_subagents,
        error_events,
        rate_limit_events,
        truncated,
        peak_context_tokens,
        cache_read_tokens,
        ctx_pct,
        model,
        repeated_tools,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyzes_repeated_tools_nested_subagents_and_peak_context() {
        let lines = vec![
            r#"{"ts":1,"type":"tool_use","tool":"Read","target":"src/lib.rs"}"#.to_string(),
            r#"{"ts":2,"type":"tool_use","tool":"Read","target":"src/lib.rs"}"#.to_string(),
            r#"{"ts":3,"type":"tool_use","tool":"Read","target":"src/lib.rs"}"#.to_string(),
            r#"{"ts":4,"type":"tool_use","tool":"Edit","target":"src/a.rs"}"#.to_string(),
            r#"{"ts":5,"type":"tool_use","tool":"Edit","target":"src/a.rs"}"#.to_string(),
            r#"{"ts":6,"type":"tool_use","tool":"Agent","target":"explore repo"}"#.to_string(),
            r#"{"ts":7,"type":"error","target":"hook denied"}"#.to_string(),
            r#"{"ts":8,"type":"rate_limit","target":"rejected"}"#.to_string(),
            r#"{"ts":9,"type":"truncated"}"#.to_string(),
            "not valid json".to_string(),
            r#"{"ts":10,"type":"finish","tool":"claude","target":"exit=0 timed_out=false tokens_in=20000 tokens_out=4000 peak_ctx=170000 cache_read=150000 ctx_pct=85% model=claude-sonnet-4-6"}"#.to_string(),
        ];
        let report = analyze_stream_lines("r-1", &lines);
        assert_eq!(report.tool_calls, 6);
        assert_eq!(report.nested_subagents, 1);
        assert_eq!(report.error_events, 1);
        assert_eq!(report.rate_limit_events, 1);
        assert!(report.truncated);
        assert_eq!(report.peak_context_tokens, Some(170_000));
        assert_eq!(report.cache_read_tokens, Some(150_000));
        assert_eq!(report.ctx_pct, Some(85));
        assert_eq!(report.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(
            report.repeated_tools,
            vec![
                RepeatedTool {
                    tool: "Read".to_string(),
                    target: "src/lib.rs".to_string(),
                    count: 3,
                },
                RepeatedTool {
                    tool: "Edit".to_string(),
                    target: "src/a.rs".to_string(),
                    count: 2,
                },
            ]
        );
    }
}
