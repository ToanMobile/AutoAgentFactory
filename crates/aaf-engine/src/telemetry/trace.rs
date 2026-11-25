//! Per-task execution trace: chronological runs and events with tokens,
//! wall-clock and (API-billed accounts only) estimated cost.

use aaf_domain::event::EventKind;
use aaf_domain::ports::{Clock, EventSink, PortResult, RunStore, TaskStore};
use aaf_domain::task::TaskState;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{CostBasis, CostPolicy};
use crate::clock::SystemClock;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceEntry {
    pub timestamp: i64,
    pub source: String,
    pub kind: String,
    pub summary: String,
    pub run_id: Option<String>,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskTrace {
    pub task_id: String,
    pub title: String,
    pub state: TaskState,
    pub priority: String,
    pub total_tokens_in: u64,
    pub total_tokens_out: u64,
    pub total_wall_secs: u64,
    pub estimated_cost_usd: Option<f64>,
    pub cost_basis: CostBasis,
    pub events_truncated: bool,
    pub entries: Vec<TraceEntry>,
}

/// Builds a chronological run + event timeline for `task_id` using the current wall clock.
pub fn collect_trace(
    tasks: &dyn TaskStore,
    runs: &dyn RunStore,
    events: &dyn EventSink,
    task_id: &str,
    cost: &CostPolicy,
    limit: u32,
) -> PortResult<TaskTrace> {
    let now = SystemClock.now();
    collect_trace_at(tasks, runs, events, task_id, cost, limit, now)
}

/// Builds a chronological run + event timeline for `task_id` at timestamp `now`.
pub fn collect_trace_at(
    tasks: &dyn TaskStore,
    runs: &dyn RunStore,
    events: &dyn EventSink,
    task_id: &str,
    cost: &CostPolicy,
    limit: u32,
    now: i64,
) -> PortResult<TaskTrace> {
    let task = tasks.get_task(task_id)?;
    let run_list = runs.runs_for_task(task_id)?;
    let fetch_limit = limit.saturating_add(1);
    let mut event_list = events.events_for_task(task_id, fetch_limit)?;
    let limit_usize = usize::try_from(limit).unwrap_or(usize::MAX);
    let events_truncated = event_list.len() > limit_usize;
    if events_truncated {
        event_list.truncate(limit_usize);
    }
    // `events_for_task` returns `ORDER BY id DESC`; reverse to chronological order before merge.
    event_list.reverse();

    let (mut total_tokens_in, mut total_tokens_out, mut total_wall_secs) = (0u64, 0u64, 0u64);
    let mut entries = Vec::with_capacity(run_list.len() + event_list.len());

    for run in &run_list {
        total_tokens_in = total_tokens_in.saturating_add(run.tokens_in);
        total_tokens_out = total_tokens_out.saturating_add(run.tokens_out);
        let end = run.ended_at.unwrap_or(now);
        let wall = u64::try_from(end.saturating_sub(run.started_at)).unwrap_or(0);
        total_wall_secs = total_wall_secs.saturating_add(wall);
        let (rid, exec, st) = (&run.id, &run.executor, run.status.as_str());
        let (tin, tout) = (run.tokens_in, run.tokens_out);
        entries.push(TraceEntry {
            timestamp: run.started_at,
            source: "run".to_string(),
            kind: st.to_string(),
            summary: format!("run {rid} [{exec}] {st} ({tin} in / {tout} out, {wall}s)"),
            run_id: Some(rid.clone()),
            payload: json!({
                "executor": exec,
                "branch": run.branch,
                "account_id": run.account_id,
                "exit_code": run.exit_code,
                "tokens_in": tin,
                "tokens_out": tout,
                "wall_secs": wall,
            }),
        });
    }

    for event in &event_list {
        let payload = serde_json::from_str::<Value>(&event.payload_json)
            .unwrap_or_else(|_| Value::String(event.payload_json.clone()));
        entries.push(TraceEntry {
            timestamp: event.created_at,
            source: "event".to_string(),
            kind: event.kind.as_str().to_string(),
            summary: summarize_event_one_line(event.kind, &event.payload_json),
            run_id: event.run_id.clone(),
            payload,
        });
    }

    entries.sort_by_key(|e| {
        (
            e.timestamp,
            match e.source.as_str() {
                "run" => 0u8,
                "event" => 1,
                _ => 2,
            },
        )
    });

    let estimated_cost_usd = cost.estimate(
        run_list
            .iter()
            .map(|r| (r.account_id.as_deref(), r.tokens_in, r.tokens_out)),
    );
    let cost_basis = CostBasis::from_estimate(estimated_cost_usd);
    Ok(TaskTrace {
        task_id: task.id,
        title: task.title,
        state: task.state,
        priority: task.priority.as_str().to_string(),
        total_tokens_in,
        total_tokens_out,
        total_wall_secs,
        estimated_cost_usd,
        cost_basis,
        events_truncated,
        entries,
    })
}

/// Concise single-line human summary for an event (never dumps 32 KiB contracts).
pub fn summarize_event_one_line(kind: EventKind, payload_json: &str) -> String {
    let tag = kind.as_str();
    let Ok(v) = serde_json::from_str::<Value>(payload_json) else {
        return format!("{tag}: {}", truncate_one_line(payload_json, 160));
    };
    let s = |k: &str| v[k].as_str().unwrap_or("");
    let opt = |k: &str| v[k].as_str().filter(|x| !x.is_empty());
    let u = |k: &str| v[k].as_u64().unwrap_or(0);
    let sum = opt("summary")
        .or_else(|| v["plan_contract"]["summary"].as_str())
        .unwrap_or("");
    let h12: String = s("plan_contract_hash").chars().take(12).collect();
    match kind {
        EventKind::PlanDebateTurn => {
            let (r, role, ok) = (
                u("round"),
                opt("role").unwrap_or("?"),
                v["ok"].as_bool().unwrap_or(false),
            );
            let ver = opt("verdict")
                .or_else(|| v["critique"]["verdict"].as_str())
                .unwrap_or("");
            let mut out = format!("{tag}: r{r} {role} ok={ok}");
            if !ver.is_empty() {
                out.push_str(&format!(" verdict={ver}"));
            }
            let open = v["open_count"]
                .as_u64()
                .or_else(|| v["open"].as_u64())
                .or_else(|| v["open_issues"].as_array().map(|a| a.len() as u64));
            if let Some(n) = open {
                out.push_str(&format!(" open={n}"));
            }
            if !sum.is_empty() {
                out.push_str(&format!(" summary={}", truncate_one_line(sum, 120)));
            }
            out
        }
        EventKind::PlanConverged | EventKind::PlanGenerated
            if kind == EventKind::PlanConverged || !sum.is_empty() || !h12.is_empty() =>
        {
            let mut out = format!("{tag}: {}", truncate_one_line(sum, 140));
            if !h12.is_empty() {
                out.push_str(&format!(" hash={h12}"));
            }
            let fb = s("forced_by");
            if kind == EventKind::PlanConverged && !fb.is_empty() {
                out.push_str(&format!(" forced_by={fb}"));
            }
            out
        }
        EventKind::PlanGenerated => {
            let body = opt("plan").unwrap_or(payload_json);
            format!("{tag}: {}", truncate_one_line(body, 160))
        }
        EventKind::DualReviewTurn if opt("phase") == Some("adjudicate") => {
            let (sv, dp) = (u("surviving"), u("dropped_p1"));
            format!("{tag}: phase=adjudicate surviving={sv} dropped_p1={dp}")
        }
        EventKind::DualReviewTurn => {
            let id = opt("reviewer_id").unwrap_or_else(|| s("reviewer"));
            let (ex, m, ver) = (s("executor"), s("model"), s("verdict"));
            let (p0, p1, p2) = (u("p0"), u("p1"), u("p2"));
            format!("{tag}: reviewer={id} ({ex}/{m}) verdict={ver} p0={p0} p1={p1} p2={p2}")
        }
        EventKind::ImplementerEscalated => {
            let (tier, ex, m, eff) = (s("tier"), s("executor"), s("model"), s("effort"));
            let rw = v["reworks"]
                .as_u64()
                .or_else(|| v["rework_count"].as_u64())
                .unwrap_or(0);
            format!("{tag}: tier={tier} reworks={rw} -> {ex}/{m} ({eff})")
        }
        _ => format!("{tag}: {}", truncate_one_line(payload_json, 220)),
    }
}

fn truncate_one_line(raw: &str, max_chars: usize) -> String {
    let (mut out, mut n, mut ws) = (String::new(), 0usize, true);
    for ch in raw.chars() {
        let is_ws = ch.is_whitespace() || ch.is_control();
        if is_ws && ws {
            continue;
        }
        if n >= max_chars {
            out.push_str("...");
            break;
        }
        out.push(if is_ws { ' ' } else { ch });
        n += 1;
        ws = is_ws;
    }
    out.trim_end().to_string()
}
