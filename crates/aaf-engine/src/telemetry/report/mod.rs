//! Period report: what the factory finished, what it cost, and where the
//! time went (`aaf report`). Read-only, built from tasks, runs and the
//! `task_state_changed` events every state move already records.

use std::collections::{BTreeMap, HashMap, HashSet};

use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{PortResult, RunStore, TaskStore};
use aaf_domain::run::{RunRecord, RunStatus};
use aaf_domain::task::{Task, TaskState};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    AutonomyMetrics, CostBasis, CostPolicy, TaskCounts, bump_task, compute_autonomy_metrics,
};
use crate::db::Db;

/// Label of the time from task creation to its first recorded state move.
pub const QUEUED_STAGE: &str = "queued";

/// How many blocked reasons the report lists.
const TOP_BLOCKED: usize = 5;

/// The period a report covers: `since..=now`, or everything when `since` is `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportWindow {
    pub since: Option<i64>,
    pub now: i64,
}

impl ReportWindow {
    fn covers(self, ts: i64) -> bool {
        self.since.is_none_or(|since| ts >= since)
    }
}

/// Spread of a set of durations, in seconds (nearest-rank percentiles).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurationStats {
    pub samples: u64,
    pub total_secs: u64,
    pub p50_secs: u64,
    pub p90_secs: u64,
    pub max_secs: u64,
}

impl DurationStats {
    pub fn from_samples(mut samples: Vec<u64>) -> DurationStats {
        if samples.is_empty() {
            return DurationStats::default();
        }
        samples.sort_unstable();
        let total = samples.iter().fold(0u64, |acc, s| acc.saturating_add(*s));
        DurationStats {
            samples: samples.len() as u64,
            total_secs: total,
            p50_secs: nearest_rank(&samples, 50),
            p90_secs: nearest_rank(&samples, 90),
            max_secs: samples.last().copied().unwrap_or(0),
        }
    }
}

/// `samples` must be sorted and non-empty.
fn nearest_rank(samples: &[u64], pct: usize) -> u64 {
    let rank = (pct * samples.len()).div_ceil(100).max(1);
    samples
        .get(rank.saturating_sub(1))
        .copied()
        .unwrap_or_default()
}

/// Time spent in one task state, over every closed stay that started in the window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageStat {
    pub stage: String,
    pub durations: DurationStats,
    /// Tasks sitting in this state right now (stays not closed yet).
    pub open_now: u64,
}

/// Runs, tokens, wall time and approximate cost of one executor (USD only
/// for runs on API-billed accounts; `None` when it has none).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutorStat {
    pub executor: String,
    pub runs: u64,
    pub succeeded: u64,
    pub failed: u64,
    pub running: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub wall_secs: u64,
    pub estimated_cost_usd: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FactoryReport {
    pub window: ReportWindow,
    /// Tasks created or updated inside the window, by current state.
    pub counts: TaskCounts,
    /// Tasks whose move to `done` happened inside the window.
    pub tasks_done: u64,
    /// Creation to `done`, for the tasks counted in `tasks_done`.
    pub lead_time: DurationStats,
    /// In lifecycle order; states with no data are left out.
    pub stages: Vec<StageStat>,
    pub executors: Vec<ExecutorStat>,
    pub total_tokens_in: u64,
    pub total_tokens_out: u64,
    /// Approximate USD for runs on API-billed accounts only; `None` when the
    /// period has none (subscription accounts report tokens only).
    pub estimated_cost_usd: Option<f64>,
    #[serde(default)]
    pub cost_basis: CostBasis,
    /// `(reason kind, tasks)` of the tasks blocked now, most frequent first.
    pub top_blocked: Vec<(String, u64)>,
    pub autonomy: AutonomyMetrics,
}

/// Reads tasks, runs and events of `repo_root` (or every repository) and builds the report.
pub fn collect_report(
    db: &Db,
    repo_root: Option<&str>,
    window: ReportWindow,
    cost: &CostPolicy,
) -> PortResult<FactoryReport> {
    let tasks: Vec<Task> = db
        .list_tasks(repo_root, None)?
        .into_iter()
        .filter(|t| in_window(t, window))
        .collect();
    let mut runs = Vec::new();
    for task in &tasks {
        runs.extend(db.runs_for_task(&task.id)?);
    }
    let ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
    let events: Vec<FactoryEvent> = db
        .recent_events(repo_root, u32::MAX)?
        .into_iter()
        .filter(|e| e.task_id.as_deref().is_some_and(|id| ids.contains(id)))
        .collect();
    Ok(compute_report(&tasks, &runs, &events, window, cost))
}

fn in_window(task: &Task, window: ReportWindow) -> bool {
    window.covers(task.created_at) || window.covers(task.updated_at)
}

/// Pure aggregation; tasks outside `window` are ignored.
pub fn compute_report(
    tasks: &[Task],
    runs: &[RunRecord],
    events: &[FactoryEvent],
    window: ReportWindow,
    cost: &CostPolicy,
) -> FactoryReport {
    let tasks: Vec<&Task> = tasks.iter().filter(|t| in_window(t, window)).collect();
    let ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
    let moves = state_moves(events, &ids);

    let mut counts = TaskCounts::default();
    let mut blocked: BTreeMap<String, u64> = BTreeMap::new();
    let mut stage_samples: HashMap<String, Vec<u64>> = HashMap::new();
    let mut open_now: HashMap<String, u64> = HashMap::new();
    let mut lead_samples = Vec::new();
    let mut done_ids = HashSet::new();

    for task in &tasks {
        bump_task(&mut counts, task.state, 1);
        if task.state == TaskState::Blocked {
            let kind = task.blocked_reason.as_ref().map_or("unknown", |r| r.kind());
            *blocked.entry(kind.to_string()).or_default() += 1;
        }
        let task_moves = moves.get(task.id.as_str()).map_or(&[][..], Vec::as_slice);
        for stay in stays(task, task_moves) {
            match stay.end {
                Some(end) if window.covers(stay.start) => stage_samples
                    .entry(stay.stage)
                    .or_default()
                    .push(secs_between(stay.start, end)),
                Some(_) => {}
                None if stay.stage == task.state.as_str() => {
                    *open_now.entry(stay.stage).or_default() += 1;
                }
                None => {}
            }
        }
        if task.state == TaskState::Done {
            done_ids.insert(task.id.clone());
            let done_at = task_moves
                .iter()
                .rev()
                .find(|(_, to)| *to == TaskState::Done)
                .map_or(task.updated_at, |(ts, _)| *ts);
            if window.covers(done_at) {
                lead_samples.push(secs_between(task.created_at, done_at));
            }
        }
    }

    let in_scope = |r: &&RunRecord| ids.contains(r.task_id.as_str()) && window.covers(r.started_at);
    let executors = executor_stats(runs, &ids, window, cost);
    let estimated_cost_usd = cost.estimate(
        runs.iter()
            .filter(in_scope)
            .map(|r| (r.account_id.as_deref(), r.tokens_in, r.tokens_out)),
    );
    let total_tokens_in = executors.iter().map(|e| e.tokens_in).sum();
    let total_tokens_out = executors.iter().map(|e| e.tokens_out).sum();
    let scoped_events: Vec<FactoryEvent> = events
        .iter()
        .filter(|e| e.task_id.as_deref().is_some_and(|id| ids.contains(id)))
        .cloned()
        .collect();

    FactoryReport {
        window,
        counts,
        tasks_done: lead_samples.len() as u64,
        lead_time: DurationStats::from_samples(lead_samples),
        stages: stage_stats(stage_samples, open_now),
        executors,
        total_tokens_in,
        total_tokens_out,
        estimated_cost_usd,
        cost_basis: CostBasis::from_estimate(estimated_cost_usd),
        top_blocked: top_blocked(blocked),
        autonomy: compute_autonomy_metrics(&done_ids, &scoped_events),
    }
}

/// `(timestamp, target state)` of each recorded move, per task, oldest first.
fn state_moves<'a>(
    events: &'a [FactoryEvent],
    ids: &HashSet<&str>,
) -> HashMap<&'a str, Vec<(i64, TaskState)>> {
    let mut ordered: Vec<&FactoryEvent> = events
        .iter()
        .filter(|e| e.kind == EventKind::TaskStateChanged)
        .collect();
    ordered.sort_by_key(|e| (e.created_at, e.id.unwrap_or(0)));
    let mut moves: HashMap<&str, Vec<(i64, TaskState)>> = HashMap::new();
    for event in ordered {
        let Some(task_id) = event.task_id.as_deref().filter(|id| ids.contains(id)) else {
            continue;
        };
        if let Some(to) = target_state(&event.payload_json) {
            moves
                .entry(task_id)
                .or_default()
                .push((event.created_at, to));
        }
    }
    moves
}

fn target_state(payload_json: &str) -> Option<TaskState> {
    let value: Value = serde_json::from_str(payload_json).ok()?;
    TaskState::parse(value.get("to")?.as_str()?)
}

struct Stay {
    stage: String,
    start: i64,
    /// `None` while the task is still in this stage.
    end: Option<i64>,
}

/// Consecutive stays of a task: `queued` from creation to the first move,
/// then one stay per move. A terminal state has no stay.
fn stays(task: &Task, moves: &[(i64, TaskState)]) -> Vec<Stay> {
    let mut out = Vec::with_capacity(moves.len() + 1);
    let first_end = moves.first().map(|(ts, _)| *ts);
    out.push(Stay {
        stage: QUEUED_STAGE.to_string(),
        start: task.created_at,
        end: first_end,
    });
    for (i, (start, state)) in moves.iter().enumerate() {
        if state.is_terminal() {
            continue;
        }
        out.push(Stay {
            stage: state.as_str().to_string(),
            start: *start,
            end: moves.get(i + 1).map(|(ts, _)| *ts),
        });
    }
    out
}

fn secs_between(start: i64, end: i64) -> u64 {
    u64::try_from(end.saturating_sub(start)).unwrap_or(0)
}

fn stage_stats(
    mut samples: HashMap<String, Vec<u64>>,
    mut open_now: HashMap<String, u64>,
) -> Vec<StageStat> {
    let order = std::iter::once(QUEUED_STAGE).chain(TaskState::ALL.iter().map(|s| s.as_str()));
    let mut out = Vec::new();
    for stage in order {
        let durations = DurationStats::from_samples(samples.remove(stage).unwrap_or_default());
        let open = open_now.remove(stage).unwrap_or(0);
        if durations.samples == 0 && open == 0 {
            continue;
        }
        out.push(StageStat {
            stage: stage.to_string(),
            durations,
            open_now: open,
        });
    }
    out
}

fn executor_stats(
    runs: &[RunRecord],
    ids: &HashSet<&str>,
    window: ReportWindow,
    cost: &CostPolicy,
) -> Vec<ExecutorStat> {
    let mut by_exec: BTreeMap<&str, ExecutorStat> = BTreeMap::new();
    let in_scope: Vec<&RunRecord> = runs
        .iter()
        .filter(|r| ids.contains(r.task_id.as_str()) && window.covers(r.started_at))
        .collect();
    for run in &in_scope {
        let stat = by_exec
            .entry(run.executor.as_str())
            .or_insert_with(|| ExecutorStat {
                executor: run.executor.clone(),
                runs: 0,
                succeeded: 0,
                failed: 0,
                running: 0,
                tokens_in: 0,
                tokens_out: 0,
                wall_secs: 0,
                estimated_cost_usd: None,
            });
        stat.runs += 1;
        match run.status {
            RunStatus::Succeeded => stat.succeeded += 1,
            RunStatus::Running => stat.running += 1,
            _ => stat.failed += 1,
        }
        stat.tokens_in = stat.tokens_in.saturating_add(run.tokens_in);
        stat.tokens_out = stat.tokens_out.saturating_add(run.tokens_out);
        let end = run.ended_at.unwrap_or(window.now);
        stat.wall_secs = stat
            .wall_secs
            .saturating_add(secs_between(run.started_at, end));
    }
    by_exec
        .into_values()
        .map(|mut stat| {
            stat.estimated_cost_usd = cost.estimate(
                in_scope
                    .iter()
                    .filter(|r| r.executor == stat.executor)
                    .map(|r| (r.account_id.as_deref(), r.tokens_in, r.tokens_out)),
            );
            stat
        })
        .collect()
}

fn top_blocked(blocked: BTreeMap<String, u64>) -> Vec<(String, u64)> {
    let mut list: Vec<(String, u64)> = blocked.into_iter().collect();
    // Stable: ties keep the alphabetical order of the map.
    list.sort_by_key(|a| std::cmp::Reverse(a.1));
    list.truncate(TOP_BLOCKED);
    list
}

#[cfg(test)]
mod tests;
