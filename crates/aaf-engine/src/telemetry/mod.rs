//! Read-only telemetry dashboard and per-task execution trace (OpenAgentd essence).

use std::collections::BTreeMap;
use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};

use aaf_domain::account::AccountState;
use aaf_domain::event::EventKind;
use aaf_domain::ports::{
    AccountStore, Clock, EventSink, KvStore, META_CONSECUTIVE_ROOT_FAILS, META_FACTORY_FROZEN,
    PortResult, RunStore, TaskStore,
};
use aaf_domain::run::RunStatus;
use aaf_domain::task::TaskState;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::clock::SystemClock;
use crate::db::Db;

mod autonomy;
mod report;
pub use autonomy::{
    AutonomyMetrics, CostRates, collect_autonomy_metrics, collect_autonomy_metrics_for_tasks,
    compute_autonomy_metrics,
};
pub use report::{
    DurationStats, ExecutorStat, FactoryReport, ReportWindow, StageStat, collect_report,
    compute_report,
};

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
    #[serde(default)]
    pub autonomy: AutonomyMetrics,
}

/// Probes whether `path` is a regular file currently held under an exclusive
/// advisory lock. Uses a non-blocking shared lock and unlocks immediately.
pub(crate) fn is_exclusively_locked(path: &Path) -> bool {
    let is_reg =
        fs::symlink_metadata(path).is_ok_and(|m| !m.file_type().is_symlink() && m.is_file());
    if !is_reg {
        return false;
    }
    File::open(path).is_ok_and(|file| match file.try_lock_shared() {
        Ok(()) => {
            let _ = file.unlock();
            false
        }
        Err(TryLockError::WouldBlock) => true,
        Err(TryLockError::Error(_)) => false,
    })
}

/// Raw result of probing a single `heavy.<n>.lock` entry in `<data_dir>/throttle`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeavySlotEntry {
    pub slot: usize,
    pub path: PathBuf,
    pub locked: bool,
}

/// Scans `<data_dir>/throttle` once (single `read_dir` + `flock` pass) and
/// returns both the aggregate [`ThrottleSnapshot`] and per-slot probe results.
pub fn scan_throttle(data_dir: &Path) -> (ThrottleSnapshot, Vec<HeavySlotEntry>) {
    let dir = data_dir.join("throttle");
    if !dir.is_dir() {
        return (ThrottleSnapshot::default(), Vec::new());
    }
    let drain_lock_present = is_exclusively_locked(&dir.join("drain.lock"));
    let (mut slot_lock_files, mut throttle_slots, mut slot_entries) = (0usize, 0usize, Vec::new());
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Some(slot_str) = name
                .strip_prefix("heavy.")
                .and_then(|s| s.strip_suffix(".lock"))
            {
                slot_lock_files = slot_lock_files.saturating_add(1);
                let path = entry.path();
                let locked = is_exclusively_locked(&path);
                throttle_slots = throttle_slots.saturating_add(usize::from(locked));
                if let Ok(slot) = slot_str.parse::<usize>() {
                    slot_entries.push(HeavySlotEntry { slot, path, locked });
                }
            }
        }
    }
    let guest_record_files = fs::read_dir(dir.join("guest")).map_or(0, |entries| {
        let count = entries
            .flatten()
            .filter(|e| e.file_name().to_str().is_some_and(|n| n.ends_with(".rec")))
            .count();
        u32::try_from(count).unwrap_or(u32::MAX)
    });
    let snap = ThrottleSnapshot {
        throttle_dir_exists: true,
        drain_lock_present,
        throttle_slots,
        slot_lock_files,
        guest_record_files,
    };
    (snap, slot_entries)
}

/// Inspects `<data_dir>/throttle` read-only using non-blocking shared lock probes.
pub fn inspect_throttle(data_dir: &Path) -> ThrottleSnapshot {
    scan_throttle(data_dir).0
}

/// Checks `<data_dir>/leases/<account_id>.lock` (and extra `.slot<n>.lock` files)
/// read-only via non-blocking shared lock probe.
pub(crate) fn is_account_leased(leases_dir: &Path, account_id: &str) -> bool {
    let safe_stem = !account_id.is_empty()
        && !account_id.starts_with('.')
        && account_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
    if !safe_stem || !leases_dir.is_dir() {
        return false;
    }
    if is_exclusively_locked(&leases_dir.join(format!("{account_id}.lock"))) {
        return true;
    }
    let prefix = format!("{account_id}.slot");
    fs::read_dir(leases_dir).is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".lock"))
                && is_exclusively_locked(&entry.path())
        })
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

#[derive(Default)]
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
    let mut totals = RunTotals::default();
    for task in &all_tasks {
        for run in runs.runs_for_task(&task.id)? {
            bump_run(&mut totals.counts, run.status, 1);
            totals.tokens_in = totals.tokens_in.saturating_add(run.tokens_in);
            totals.tokens_out = totals.tokens_out.saturating_add(run.tokens_out);
        }
    }
    let groups: Vec<_> = all_tasks
        .iter()
        .map(|t| (t.state, t.blocked_reason.as_ref().map(|r| r.kind()), 1))
        .collect();
    let throttle = query.data_dir.map(inspect_throttle).unwrap_or_default();
    // Without an EventSink, task event history is unavailable so done tasks are
    // omitted from autonomy denominators rather than assumed 100% autonomous.
    let autonomy = AutonomyMetrics::default();
    assemble_status(&groups, totals, kv, accounts, query, throttle, autonomy)
}

/// Collects global/repo status directly from [`Db`] using a single SQL
/// aggregation for runs/tokens instead of per-task queries.
pub fn collect_status_from_db(db: &Db, query: &StatusQuery<'_>) -> PortResult<TelemetrySummary> {
    let throttle = query.data_dir.map(inspect_throttle).unwrap_or_default();
    collect_status_from_db_with_throttle(db, query, throttle)
}

/// Collects global/repo status from [`Db`] reusing a pre-scanned [`ThrottleSnapshot`].
pub fn collect_status_from_db_with_throttle(
    db: &Db,
    query: &StatusQuery<'_>,
    throttle: ThrottleSnapshot,
) -> PortResult<TelemetrySummary> {
    let groups = db.task_state_counts(query.repo_root)?;
    let has_done = groups
        .iter()
        .any(|(s, _, n)| *s == TaskState::Done && *n > 0);
    let mut totals = RunTotals::default();
    for (status, count, t_in, t_out) in db.run_totals_by_repo(query.repo_root)? {
        bump_run(
            &mut totals.counts,
            status,
            u32::try_from(count).unwrap_or(u32::MAX),
        );
        totals.tokens_in = totals.tokens_in.saturating_add(t_in);
        totals.tokens_out = totals.tokens_out.saturating_add(t_out);
    }
    let autonomy =
        autonomy::collect_autonomy_metrics_with_done_hint(db, query.repo_root, has_done)?;
    assemble_status(&groups, totals, db, Some(db), query, throttle, autonomy)
}

/// `task_groups`: `(state, blocked kind, task count)`.
fn assemble_status(
    task_groups: &[(TaskState, Option<&'static str>, usize)],
    totals: RunTotals,
    kv: &dyn KvStore,
    accounts: Option<&dyn AccountStore>,
    query: &StatusQuery<'_>,
    throttle: ThrottleSnapshot,
    autonomy: AutonomyMetrics,
) -> PortResult<TelemetrySummary> {
    let frozen = kv.get_meta(META_FACTORY_FROZEN)?.as_deref() == Some("1");
    let consecutive_root_fails = kv
        .get_meta(META_CONSECUTIVE_ROOT_FAILS)?
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0);
    let (mut counts, mut blocked_reasons) = (TaskCounts::default(), BTreeMap::new());
    for &(state, kind, n) in task_groups {
        bump_task(&mut counts, state, u32::try_from(n).unwrap_or(u32::MAX));
        if state == TaskState::Blocked {
            let entry = blocked_reasons
                .entry(kind.unwrap_or("unknown").to_string())
                .or_insert(0usize);
            *entry = entry.saturating_add(n);
        }
    }
    let leases_dir = query.data_dir.map(|dir| dir.join("leases"));
    let account_list = match accounts {
        Some(s) => summarize_accounts(&s.list_accounts()?, leases_dir.as_deref(), query.now),
        None => Vec::new(),
    };
    let estimated_cost_usd = query
        .rates
        .estimate_usd(totals.tokens_in, totals.tokens_out);
    Ok(TelemetrySummary {
        frozen,
        consecutive_root_fails,
        repo_root: query.repo_root.map(str::to_string),
        counts,
        blocked_reasons,
        runs: totals.counts,
        tokens_in: totals.tokens_in,
        tokens_out: totals.tokens_out,
        estimated_cost_usd,
        accounts: account_list,
        throttle,
        autonomy,
    })
}

pub(crate) fn summarize_accounts(
    states: &[AccountState],
    leases_dir: Option<&Path>,
    now: i64,
) -> Vec<AccountSummary> {
    states
        .iter()
        .map(|acct| {
            let status = if acct.in_cooldown(now) {
                "cooldown"
            } else if leases_dir.is_some_and(|dir| is_account_leased(dir, &acct.profile.id)) {
                "leased"
            } else {
                "available"
            };
            AccountSummary {
                id: acct.profile.id.clone(),
                name: acct.profile.name.clone(),
                provider: acct.profile.provider.as_str().to_string(),
                status: status.to_string(),
                cooldown_until: acct.cooldown_until,
            }
        })
        .collect()
}

pub(crate) fn bump_task(counts: &mut TaskCounts, state: TaskState, n: u32) {
    counts.total = counts.total.saturating_add(n);
    let slot = match state {
        TaskState::Backlog => &mut counts.backlog,
        TaskState::Ready => &mut counts.ready,
        TaskState::Running => &mut counts.running,
        TaskState::Gating => &mut counts.gating,
        TaskState::Review => &mut counts.review,
        TaskState::Blocked => &mut counts.blocked,
        TaskState::Done => &mut counts.done,
        TaskState::Cancelled => &mut counts.cancelled,
    };
    *slot = slot.saturating_add(n);
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
    let now = SystemClock.now();
    collect_trace_at(tasks, runs, events, task_id, rates, limit, now)
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
