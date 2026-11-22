//! Read-only telemetry dashboard and per-task execution trace (OpenAgentd essence).

use std::collections::BTreeMap;
use std::fs::{self, File, TryLockError};
use std::path::Path;

use aaf_domain::ports::{
    AccountStore, Clock, EventSink, KvStore, META_CONSECUTIVE_ROOT_FAILS, META_FACTORY_FROZEN,
    PortResult, RunStore, TaskStore,
};
use aaf_domain::run::RunStatus;
use aaf_domain::task::{Task, TaskState};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::db::Db;
use crate::factory::adapters::SystemClock;

/// Per-million-token pricing used for approximate cost display.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CostRates {
    pub input_per_million_usd: f64,
    pub output_per_million_usd: f64,
}

impl Default for CostRates {
    fn default() -> Self {
        Self {
            input_per_million_usd: 3.0,
            output_per_million_usd: 15.0,
        }
    }
}

impl CostRates {
    pub fn estimate_usd(self, tokens_in: u64, tokens_out: u64) -> f64 {
        let input = (tokens_in as f64 / 1_000_000.0) * self.input_per_million_usd;
        let output = (tokens_out as f64 / 1_000_000.0) * self.output_per_million_usd;
        ((input + output) * 1_000_000.0).round() / 1_000_000.0
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskCounts {
    pub backlog: u32,
    pub ready: u32,
    pub running: u32,
    pub gating: u32,
    pub review: u32,
    pub blocked: u32,
    pub done: u32,
    pub cancelled: u32,
    pub total: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunCounts {
    pub total: u32,
    pub running: u32,
    pub succeeded: u32,
    pub failed: u32,
    pub timed_out: u32,
    pub limit_hit: u32,
    pub killed: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountSummary {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub status: String,
    pub cooldown_until: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThrottleSnapshot {
    pub throttle_dir_exists: bool,
    /// True only when `drain.lock` is actively locked by a live holder.
    pub drain_lock_present: bool,
    /// Count of `heavy.*.lock` files actively locked by a live holder.
    pub throttle_slots: usize,
    /// Total `heavy.*.lock` files present on disk (active or idle).
    pub slot_lock_files: usize,
    pub guest_record_files: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TelemetrySummary {
    pub frozen: bool,
    pub consecutive_root_fails: u32,
    pub repo_root: Option<String>,
    pub counts: TaskCounts,
    pub blocked_reasons: BTreeMap<String, usize>,
    pub runs: RunCounts,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub estimated_cost_usd: f64,
    pub accounts: Vec<AccountSummary>,
    pub throttle: ThrottleSnapshot,
}

/// Probes whether `path` is a regular file currently held under an exclusive
/// advisory lock. Uses a non-blocking shared lock and unlocks immediately.
fn is_exclusively_locked(path: &Path) -> bool {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return false;
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        return false;
    }
    let Ok(file) = File::open(path) else {
        return false;
    };
    match file.try_lock_shared() {
        Ok(()) => {
            let _ = file.unlock();
            false
        }
        Err(TryLockError::WouldBlock) => true,
        Err(TryLockError::Error(_)) => false,
    }
}

/// Inspects `<data_dir>/throttle` read-only using non-blocking shared lock probes.
pub fn inspect_throttle(data_dir: &Path) -> ThrottleSnapshot {
    let dir = data_dir.join("throttle");
    if !dir.is_dir() {
        return ThrottleSnapshot::default();
    }
    let drain_lock_present = is_exclusively_locked(&dir.join("drain.lock"));
    let mut slot_lock_files = 0usize;
    let mut throttle_slots = 0usize;
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.starts_with("heavy.") && name.ends_with(".lock") {
                slot_lock_files = slot_lock_files.saturating_add(1);
                if is_exclusively_locked(&entry.path()) {
                    throttle_slots = throttle_slots.saturating_add(1);
                }
            }
        }
    }
    let guest_record_files = count_matching(&dir.join("guest"), |name| name.ends_with(".rec"));
    ThrottleSnapshot {
        throttle_dir_exists: true,
        drain_lock_present,
        throttle_slots,
        slot_lock_files,
        guest_record_files,
    }
}

fn count_matching(dir: &Path, matches: fn(&str) -> bool) -> u32 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let count = entries
        .flatten()
        .filter(|entry| entry.file_name().to_str().is_some_and(matches))
        .count();
    u32::try_from(count).unwrap_or(u32::MAX)
}

fn is_safe_file_stem(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('.')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// Checks `<data_dir>/leases/<account_id>.lock` (and extra `.slot<n>.lock` files)
/// read-only via non-blocking shared lock probe.
fn is_account_leased(leases_dir: &Path, account_id: &str) -> bool {
    if !is_safe_file_stem(account_id) || !leases_dir.is_dir() {
        return false;
    }
    if is_exclusively_locked(&leases_dir.join(format!("{account_id}.lock"))) {
        return true;
    }
    let prefix = format!("{account_id}.slot");
    let Ok(entries) = fs::read_dir(leases_dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".lock"))
            && is_exclusively_locked(&entry.path())
    })
}

/// Query parameters for [`collect_status`] and [`collect_status_from_db`].
#[derive(Debug, Clone, Copy)]
pub struct StatusQuery<'a> {
    pub repo_root: Option<&'a str>,
    pub data_dir: Option<&'a Path>,
    pub now: i64,
    pub rates: CostRates,
}

struct RunTotals {
    counts: RunCounts,
    tokens_in: u64,
    tokens_out: u64,
}

/// Collects global/repo status across generic trait stores.
pub fn collect_status(
    tasks: &dyn TaskStore,
    runs: &dyn RunStore,
    kv: &dyn KvStore,
    accounts: Option<&dyn AccountStore>,
    query: &StatusQuery<'_>,
) -> PortResult<TelemetrySummary> {
    let all_tasks = tasks.list_tasks(query.repo_root, None)?;
    let mut totals = RunTotals {
        counts: RunCounts::default(),
        tokens_in: 0,
        tokens_out: 0,
    };
    for task in &all_tasks {
        for run in runs.runs_for_task(&task.id)? {
            bump_run(&mut totals.counts, run.status, 1);
            totals.tokens_in = totals.tokens_in.saturating_add(run.tokens_in);
            totals.tokens_out = totals.tokens_out.saturating_add(run.tokens_out);
        }
    }
    assemble_status(&all_tasks, totals, kv, accounts, query)
}

/// Collects global/repo status directly from [`Db`] using a single SQL
/// aggregation for runs/tokens instead of per-task queries.
pub fn collect_status_from_db(db: &Db, query: &StatusQuery<'_>) -> PortResult<TelemetrySummary> {
    let all_tasks = db.list_tasks(query.repo_root, None)?;
    let mut totals = RunTotals {
        counts: RunCounts::default(),
        tokens_in: 0,
        tokens_out: 0,
    };
    for (status, count, t_in, t_out) in db.run_totals_by_repo(query.repo_root)? {
        bump_run(
            &mut totals.counts,
            status,
            u32::try_from(count).unwrap_or(u32::MAX),
        );
        totals.tokens_in = totals.tokens_in.saturating_add(t_in);
        totals.tokens_out = totals.tokens_out.saturating_add(t_out);
    }
    assemble_status(&all_tasks, totals, db, Some(db), query)
}

fn assemble_status(
    all_tasks: &[Task],
    totals: RunTotals,
    kv: &dyn KvStore,
    accounts: Option<&dyn AccountStore>,
    query: &StatusQuery<'_>,
) -> PortResult<TelemetrySummary> {
    let frozen = kv.get_meta(META_FACTORY_FROZEN)?.as_deref() == Some("1");
    let consecutive_root_fails = kv
        .get_meta(META_CONSECUTIVE_ROOT_FAILS)?
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0);

    let mut counts = TaskCounts::default();
    let mut blocked_reasons = BTreeMap::new();
    for task in all_tasks {
        bump_task(&mut counts, task.state);
        if task.state == TaskState::Blocked {
            let kind = task.blocked_reason.as_ref().map_or("unknown", |r| r.kind());
            let entry = blocked_reasons.entry(kind.to_string()).or_insert(0usize);
            *entry = entry.saturating_add(1);
        }
    }

    let leases_dir = query.data_dir.map(|dir| dir.join("leases"));
    let account_list = match accounts {
        Some(store) => store
            .list_accounts()?
            .into_iter()
            .map(|acct| {
                let status = if acct.in_cooldown(query.now) {
                    "cooldown"
                } else if leases_dir
                    .as_deref()
                    .is_some_and(|dir| is_account_leased(dir, &acct.profile.id))
                {
                    "leased"
                } else {
                    "available"
                };
                AccountSummary {
                    id: acct.profile.id,
                    name: acct.profile.name,
                    provider: acct.profile.provider.as_str().to_string(),
                    status: status.to_string(),
                    cooldown_until: acct.cooldown_until,
                }
            })
            .collect(),
        None => Vec::new(),
    };

    let throttle = query
        .data_dir
        .map_or_else(ThrottleSnapshot::default, inspect_throttle);
    Ok(TelemetrySummary {
        frozen,
        consecutive_root_fails,
        repo_root: query.repo_root.map(str::to_string),
        counts,
        blocked_reasons,
        runs: totals.counts,
        tokens_in: totals.tokens_in,
        tokens_out: totals.tokens_out,
        estimated_cost_usd: query
            .rates
            .estimate_usd(totals.tokens_in, totals.tokens_out),
        accounts: account_list,
        throttle,
    })
}

fn bump_task(counts: &mut TaskCounts, state: TaskState) {
    counts.total = counts.total.saturating_add(1);
    match state {
        TaskState::Backlog => counts.backlog = counts.backlog.saturating_add(1),
        TaskState::Ready => counts.ready = counts.ready.saturating_add(1),
        TaskState::Running => counts.running = counts.running.saturating_add(1),
        TaskState::Gating => counts.gating = counts.gating.saturating_add(1),
        TaskState::Review => counts.review = counts.review.saturating_add(1),
        TaskState::Blocked => counts.blocked = counts.blocked.saturating_add(1),
        TaskState::Done => counts.done = counts.done.saturating_add(1),
        TaskState::Cancelled => counts.cancelled = counts.cancelled.saturating_add(1),
    }
}

fn bump_run(counts: &mut RunCounts, status: RunStatus, delta: u32) {
    counts.total = counts.total.saturating_add(delta);
    match status {
        RunStatus::Running => counts.running = counts.running.saturating_add(delta),
        RunStatus::Succeeded => counts.succeeded = counts.succeeded.saturating_add(delta),
        RunStatus::Failed => counts.failed = counts.failed.saturating_add(delta),
        RunStatus::TimedOut => counts.timed_out = counts.timed_out.saturating_add(delta),
        RunStatus::LimitHit => counts.limit_hit = counts.limit_hit.saturating_add(delta),
        RunStatus::Killed => counts.killed = counts.killed.saturating_add(delta),
    }
}

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
    pub estimated_cost_usd: f64,
    pub events_truncated: bool,
    pub entries: Vec<TraceEntry>,
}

/// Builds a chronological run + event timeline for `task_id` using the current wall clock.
pub fn collect_trace(
    tasks: &dyn TaskStore,
    runs: &dyn RunStore,
    events: &dyn EventSink,
    task_id: &str,
    rates: CostRates,
    limit: u32,
) -> PortResult<TaskTrace> {
    collect_trace_at(
        tasks,
        runs,
        events,
        task_id,
        rates,
        limit,
        SystemClock.now(),
    )
}

/// Builds a chronological run + event timeline for `task_id` at timestamp `now`.
pub fn collect_trace_at(
    tasks: &dyn TaskStore,
    runs: &dyn RunStore,
    events: &dyn EventSink,
    task_id: &str,
    rates: CostRates,
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

    let mut total_tokens_in = 0u64;
    let mut total_tokens_out = 0u64;
    let mut total_wall_secs = 0u64;
    let mut entries = Vec::with_capacity(run_list.len() + event_list.len());

    for run in &run_list {
        total_tokens_in = total_tokens_in.saturating_add(run.tokens_in);
        total_tokens_out = total_tokens_out.saturating_add(run.tokens_out);
        let end = run.ended_at.unwrap_or(now);
        let wall = u64::try_from(end.saturating_sub(run.started_at)).unwrap_or(0);
        total_wall_secs = total_wall_secs.saturating_add(wall);
        entries.push(TraceEntry {
            timestamp: run.started_at,
            source: "run".to_string(),
            kind: run.status.as_str().to_string(),
            summary: format!(
                "run {} [{}] {} ({} in / {} out, {}s)",
                run.id,
                run.executor,
                run.status.as_str(),
                run.tokens_in,
                run.tokens_out,
                wall
            ),
            run_id: Some(run.id.clone()),
            payload: json!({
                "executor": run.executor,
                "branch": run.branch,
                "account_id": run.account_id,
                "exit_code": run.exit_code,
                "tokens_in": run.tokens_in,
                "tokens_out": run.tokens_out,
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
            summary: format!("{}: {}", event.kind.as_str(), event.payload_json),
            run_id: event.run_id.clone(),
            payload,
        });
    }

    entries.sort_by(|a, b| {
        a.timestamp
            .cmp(&b.timestamp)
            .then_with(|| source_rank(&a.source).cmp(&source_rank(&b.source)))
    });

    Ok(TaskTrace {
        task_id: task.id,
        title: task.title,
        state: task.state,
        priority: task.priority.as_str().to_string(),
        total_tokens_in,
        total_tokens_out,
        total_wall_secs,
        estimated_cost_usd: rates.estimate_usd(total_tokens_in, total_tokens_out),
        events_truncated,
        entries,
    })
}

fn source_rank(source: &str) -> u8 {
    match source {
        "run" => 0,
        "event" => 1,
        _ => 2,
    }
}
