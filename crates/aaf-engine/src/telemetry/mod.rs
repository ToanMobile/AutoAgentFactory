//! Read-only telemetry dashboard and per-task execution trace (OpenAgentd essence).

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use aaf_domain::account::AccountState;
use aaf_domain::ports::{
    AccountStore, KvStore, META_CONSECUTIVE_ROOT_FAILS, META_FACTORY_FROZEN, PortResult, RunStore,
    TaskStore,
};
use aaf_domain::run::RunStatus;
use aaf_domain::task::TaskState;
use serde::{Deserialize, Serialize};

use crate::db::Db;

mod autonomy;
mod cost;
pub mod pipeline;
mod report;
pub mod score;
mod trace;
pub use autonomy::{
    AutonomyMetrics, collect_autonomy_metrics, collect_autonomy_metrics_for_tasks,
    compute_autonomy_metrics,
};
pub use cost::{CostBasis, CostPolicy, CostRates, cost_policy_for};
pub use pipeline::{
    PipelineEdge, PipelineNode, SopRunSummary, SopStageView, TaskPipeline, build_task_pipeline,
    list_sop_runs,
};
pub use report::{
    DurationStats, ExecutorStat, FactoryReport, ReportWindow, StageStat, collect_report,
    compute_report,
};
pub use score::{BottleneckHint, FactoryScore, ScorePillar, compute_factory_score};
pub use trace::{TaskTrace, TraceEntry, collect_trace, collect_trace_at, summarize_event_one_line};

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

pub(crate) use crate::throttle::inspect::is_exclusively_locked;
pub use crate::throttle::inspect::{
    HeavySlotEntry, ThrottleSnapshot, inspect_throttle, scan_throttle,
};

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
    pub estimated_cost_usd: Option<f64>,
    pub cost_basis: CostBasis,
    pub accounts: Vec<AccountSummary>,
    pub throttle: ThrottleSnapshot,
    #[serde(default)]
    pub autonomy: AutonomyMetrics,
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
    pub cost: &'a CostPolicy,
}

#[derive(Default)]
struct RunTotals {
    counts: RunCounts,
    tokens_in: u64,
    tokens_out: u64,
    per_account: Vec<(Option<String>, u64, u64)>,
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
    let mut by_account: BTreeMap<Option<String>, (u64, u64)> = BTreeMap::new();
    for task in &all_tasks {
        for run in runs.runs_for_task(&task.id)? {
            bump_run(&mut totals.counts, run.status, 1);
            totals.tokens_in = totals.tokens_in.saturating_add(run.tokens_in);
            totals.tokens_out = totals.tokens_out.saturating_add(run.tokens_out);
            let entry = by_account.entry(run.account_id).or_insert((0, 0));
            entry.0 = entry.0.saturating_add(run.tokens_in);
            entry.1 = entry.1.saturating_add(run.tokens_out);
        }
    }
    totals.per_account = by_account
        .into_iter()
        .map(|(acct, (t_in, t_out))| (acct, t_in, t_out))
        .collect();
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
    totals.per_account = db.run_tokens_by_account(query.repo_root)?;
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
    let estimated_cost_usd = query.cost.estimate(
        totals
            .per_account
            .iter()
            .map(|(acct, t_in, t_out)| (acct.as_deref(), *t_in, *t_out)),
    );
    let cost_basis = CostBasis::from_estimate(estimated_cost_usd);
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
        cost_basis,
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
