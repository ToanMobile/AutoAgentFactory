//! Accurate token-burn diagnostics, waste analysis, duplicate-audit detection,
//! and actionable alerts for the `/studio` page (`GET /api/report`).
//!
//! Pure read-only aggregation over pre-loaded tasks/runs, indexed cross-audit
//! events, root budgets, and Cost Guard state: zero LLM calls and zero schema changes.

mod alerts;
mod format;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use aaf_domain::budget::RootBudget;
use aaf_domain::pipeline::CostGuardConfig;
use aaf_domain::ports::{
    AccountStore, BudgetStore, KvStore, META_COST_PAUSED_UNTIL, META_FACTORY_FROZEN,
    META_FACTORY_UNFROZEN_AT, TaskStore,
};
use aaf_domain::run::{RunRecord, RunStatus};
use aaf_domain::task::{Task, TaskState};
use aaf_engine::db::Db;
use serde_json::Value;

use self::alerts::{AlertInputs, build_alerts, collect_duplicate_audits};
use self::format::{BurnSnapshot, build_diagnostics_payload};

const HOUR_SECS: i64 = 3600;
const DAY_SECS: i64 = 86_400;
const DEFAULT_RUN_TIMEOUT_SECS: u64 = 1800;
const STALE_RUNNING_GRACE_SECS: u64 = 900;
const STALE_RUNNING_MIN_SECS: u64 = 3600;
const TOP_RUNS_LIMIT: usize = 12;
const MAX_ALERTS: usize = 12;
pub(super) const SINGLE_RUN_SPIKE_THRESHOLD: u64 = 100_000;

/// Extra inputs resolved from the live CLI context (optional in unit tests).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct DiagnosticsCtx<'a> {
    pub data_dir: Option<&'a Path>,
    pub cost_guard: CostGuardConfig,
    pub run_timeout_secs: u64,
    pub config_error: Option<&'a str>,
}

/// Per-task token enrichment attached to `task_breakdown` rows.
#[derive(Debug, Clone, Default)]
pub(crate) struct TaskTokenExtra {
    pub wasted_tokens: u64,
    pub wasted_runs: u64,
    pub audit_tokens: u64,
    pub audit_rejected_tokens: u64,
    pub runs_timed_out: u64,
    pub runs_killed: u64,
    pub runs_limit_hit: u64,
    pub zero_token_runs: u64,
    pub duplicate_audits: u64,
    pub budget_used: Option<u64>,
    pub budget_limit: Option<u64>,
    pub budget_frozen: bool,
}

#[derive(Default)]
pub(super) struct RunTotals {
    pub(super) total_runs: u64,
    pub(super) total_in: u64,
    pub(super) total_out: u64,
    pub(super) succeeded_tok: u64,
    pub(super) running_tok: u64,
    pub(super) wasted_tok: u64,
    pub(super) wasted_runs: u64,
    pub(super) failed_tok: u64,
    pub(super) failed_runs: u64,
    pub(super) audit_rejected_tok: u64,
    pub(super) audit_rejected_runs: u64,
    pub(super) timed_out_tok: u64,
    pub(super) timed_out_runs: u64,
    pub(super) killed_tok: u64,
    pub(super) killed_runs: u64,
    pub(super) limit_hit_tok: u64,
    pub(super) limit_hit_runs: u64,
    pub(super) zero_token_runs: u64,
    pub(super) zero_token_failed_runs: u64,
    pub(super) stale_running_runs: u64,
    pub(super) audit_tok: u64,
    pub(super) audit_runs: u64,
    pub(super) coding_tok: u64,
    pub(super) coding_runs: u64,
    pub(super) other_tok: u64,
    pub(super) other_runs: u64,
}

#[derive(Default)]
pub(super) struct ExecAgg {
    pub(super) runs: u64,
    pub(super) succeeded: u64,
    pub(super) failed: u64,
    pub(super) audit_rejected: u64,
    pub(super) timed_out: u64,
    pub(super) killed: u64,
    pub(super) limit_hit: u64,
    pub(super) running: u64,
    pub(super) zero_token_runs: u64,
    pub(super) tokens_in: u64,
    pub(super) tokens_out: u64,
    pub(super) wasted_tokens: u64,
    pub(super) wall_secs: u64,
}

#[derive(Default)]
pub(super) struct AccountAgg {
    pub(super) name: String,
    pub(super) runs: u64,
    pub(super) wasted_runs: u64,
    pub(super) tokens_in: u64,
    pub(super) tokens_out: u64,
    pub(super) wasted_tokens: u64,
}

pub(super) struct TopRunRow<'a> {
    pub(super) run: &'a RunRecord,
    pub(super) task_title: String,
    pub(super) repo_name: String,
    pub(super) account_name: Option<String>,
    pub(super) total_tokens: u64,
    pub(super) duration_secs: u64,
    pub(super) wasted: bool,
    pub(super) audit_rejected: bool,
}

pub(super) struct DupAuditRow {
    pub(super) task_id: String,
    pub(super) task_title: String,
    pub(super) tree_fp: String,
    pub(super) config_hash: String,
    pub(super) count: u64,
    pub(super) fail_count: u64,
    pub(super) inconclusive_count: u64,
    pub(super) verdicts: Vec<String>,
}

/// True when a run's tokens were genuinely wasted.
/// Cross-audit runs returning FAIL/inconclusive (`RunStatus::Failed`) performed
/// real review work and are tracked in `audit_rejected_tokens`, not wasted.
pub(crate) fn is_wasted_run(executor: &str, status: RunStatus) -> bool {
    match status {
        RunStatus::TimedOut | RunStatus::LimitHit | RunStatus::Killed => true,
        RunStatus::Failed => !is_audit_executor(executor),
        RunStatus::Succeeded | RunStatus::Running => false,
    }
}

pub(crate) fn is_audit_executor(executor: &str) -> bool {
    let lower = executor.to_ascii_lowercase();
    lower == "cross-audit"
        || lower.starts_with("dual-review")
        || lower.contains("audit")
        || lower.contains("adjudicate")
}

fn is_non_coding_aux(executor: &str) -> bool {
    let lower = executor.to_ascii_lowercase();
    is_audit_executor(&lower) || lower == "plan" || lower.contains("proposer") || lower == "split"
}

pub(super) fn pct(part: u64, total: u64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    (((part as f64 / total as f64) * 1000.0).round()) / 10.0
}

/// Overflow-safe check for `part / total >= threshold_pct / 100`.
pub(super) fn ratio_ge(part: u64, total: u64, threshold_pct: u64) -> bool {
    total > 0 && u128::from(part) * 100 >= u128::from(total) * u128::from(threshold_pct)
}

fn repo_basename(repo_root: &str) -> String {
    Path::new(repo_root)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string()
}

pub(super) fn wasted_level(wasted_tok: u64, wasted_pct: f64) -> &'static str {
    if (wasted_pct >= 25.0 && wasted_tok >= 50_000) || wasted_tok >= 500_000 {
        "err"
    } else if wasted_pct >= 15.0 && wasted_tok >= 50_000 {
        "warn"
    } else {
        "ok"
    }
}

/// Returns `(token_diagnostics_json, per_task_extras)` using pre-loaded per-task
/// runs (already filtered by `started_at >= since`) so DB queries are never duplicated.
pub(crate) fn collect_token_diagnostics(
    db: &Db,
    tasks: &[Task],
    runs_by_task: &HashMap<String, Vec<RunRecord>>,
    since: Option<i64>,
    now: i64,
    dctx: DiagnosticsCtx<'_>,
) -> (Value, HashMap<String, TaskTokenExtra>) {
    let task_map: HashMap<&str, &Task> = tasks.iter().map(|t| (t.id.as_str(), t)).collect();
    let acct_names: HashMap<String, String> = db
        .list_accounts()
        .unwrap_or_default()
        .into_iter()
        .filter(|a| !a.profile.name.trim().is_empty())
        .map(|a| (a.profile.id, a.profile.name))
        .collect();

    let mut extras: HashMap<String, TaskTokenExtra> = HashMap::with_capacity(tasks.len());
    let mut budgets: HashMap<String, RootBudget> = HashMap::new();
    let mut open_roots: HashSet<String> = HashSet::new();
    let mut audit_task_ids: HashSet<String> = HashSet::new();
    let mut all_runs: Vec<&RunRecord> = Vec::new();

    for t in tasks {
        if !matches!(t.state, TaskState::Done | TaskState::Cancelled) {
            open_roots.insert(t.root_id.clone());
        }
        if !budgets.contains_key(&t.root_id)
            && let Ok(b) = db.get_budget(&t.root_id)
        {
            budgets.insert(t.root_id.clone(), b);
        }
        if let Some(runs) = runs_by_task.get(&t.id) {
            for r in runs {
                if is_audit_executor(&r.executor) {
                    audit_task_ids.insert(t.id.clone());
                }
                all_runs.push(r);
            }
        }
    }

    let dup_audits = collect_duplicate_audits(db, &task_map, &audit_task_ids, since);
    for d in &dup_audits {
        extras
            .entry(d.task_id.clone())
            .or_default()
            .duplicate_audits += d.count.saturating_sub(1);
    }

    let unfrozen_at = db
        .get_meta(META_FACTORY_UNFROZEN_AT)
        .ok()
        .flatten()
        .and_then(|s| s.trim().parse::<i64>().ok());
    let spike_since = match unfrozen_at {
        Some(ts) => now.saturating_sub(HOUR_SECS).max(ts),
        None => now.saturating_sub(HOUR_SECS),
    };
    let base_timeout = if dctx.run_timeout_secs > 0 {
        dctx.run_timeout_secs
    } else {
        DEFAULT_RUN_TIMEOUT_SECS
    };
    let stale_running_secs = base_timeout
        .saturating_add(STALE_RUNNING_GRACE_SECS)
        .max(STALE_RUNNING_MIN_SECS);

    let mut totals = RunTotals {
        total_runs: all_runs.len() as u64,
        ..RunTotals::default()
    };
    let mut by_exec: BTreeMap<String, ExecAgg> = BTreeMap::new();
    let mut by_account: BTreeMap<String, AccountAgg> = BTreeMap::new();
    let mut top_runs: Vec<TopRunRow<'_>> = Vec::new();
    let mut timeout_task_ids: Vec<String> = Vec::new();
    let mut spike_run_tasks: Vec<String> = Vec::new();
    let mut spike_run_tasks_1h: Vec<String> = Vec::new();
    let mut stale_running_tasks: Vec<String> = Vec::new();

    for r in &all_runs {
        let tot = r.tokens_in.saturating_add(r.tokens_out);
        let is_audit = is_audit_executor(&r.executor);
        let audit_rej = is_audit && r.status == RunStatus::Failed;
        let wasted = is_wasted_run(&r.executor, r.status);
        let raw_dur =
            u64::try_from(r.ended_at.unwrap_or(now).saturating_sub(r.started_at)).unwrap_or(0);
        let is_stale_running = r.status == RunStatus::Running && raw_dur > stale_running_secs;
        let dur_secs = raw_dur.min(if is_stale_running {
            stale_running_secs
        } else {
            u64::MAX
        });
        if is_stale_running {
            totals.stale_running_runs += 1;
            if !stale_running_tasks.contains(&r.task_id) {
                stale_running_tasks.push(r.task_id.clone());
            }
        }

        totals.total_in = totals.total_in.saturating_add(r.tokens_in);
        totals.total_out = totals.total_out.saturating_add(r.tokens_out);
        if tot == 0 && r.status != RunStatus::Running {
            totals.zero_token_runs += 1;
            if wasted {
                totals.zero_token_failed_runs += 1;
            }
        }
        match r.status {
            RunStatus::Succeeded => totals.succeeded_tok = totals.succeeded_tok.saturating_add(tot),
            RunStatus::Running => totals.running_tok = totals.running_tok.saturating_add(tot),
            RunStatus::Failed if audit_rej => {
                totals.audit_rejected_runs += 1;
                totals.audit_rejected_tok = totals.audit_rejected_tok.saturating_add(tot);
            }
            RunStatus::Failed => {
                totals.failed_runs += 1;
                totals.wasted_runs += 1;
                totals.failed_tok = totals.failed_tok.saturating_add(tot);
                totals.wasted_tok = totals.wasted_tok.saturating_add(tot);
            }
            RunStatus::TimedOut => {
                totals.timed_out_runs += 1;
                totals.wasted_runs += 1;
                totals.timed_out_tok = totals.timed_out_tok.saturating_add(tot);
                totals.wasted_tok = totals.wasted_tok.saturating_add(tot);
                if tot >= 30_000 && !timeout_task_ids.contains(&r.task_id) {
                    timeout_task_ids.push(r.task_id.clone());
                }
            }
            RunStatus::Killed => {
                totals.killed_runs += 1;
                totals.wasted_runs += 1;
                totals.killed_tok = totals.killed_tok.saturating_add(tot);
                totals.wasted_tok = totals.wasted_tok.saturating_add(tot);
            }
            RunStatus::LimitHit => {
                totals.limit_hit_runs += 1;
                totals.wasted_runs += 1;
                totals.limit_hit_tok = totals.limit_hit_tok.saturating_add(tot);
                totals.wasted_tok = totals.wasted_tok.saturating_add(tot);
            }
        }

        if is_audit {
            totals.audit_runs += 1;
            totals.audit_tok = totals.audit_tok.saturating_add(tot);
        } else if !is_non_coding_aux(&r.executor) {
            totals.coding_runs += 1;
            totals.coding_tok = totals.coding_tok.saturating_add(tot);
        } else {
            totals.other_runs += 1;
            totals.other_tok = totals.other_tok.saturating_add(tot);
        }

        let ea = by_exec.entry(r.executor.clone()).or_default();
        ea.runs += 1;
        ea.tokens_in = ea.tokens_in.saturating_add(r.tokens_in);
        ea.tokens_out = ea.tokens_out.saturating_add(r.tokens_out);
        ea.wall_secs = ea.wall_secs.saturating_add(dur_secs);
        if tot == 0 && r.status != RunStatus::Running {
            ea.zero_token_runs += 1;
        }
        match r.status {
            RunStatus::Succeeded => ea.succeeded += 1,
            RunStatus::Running => ea.running += 1,
            RunStatus::Failed if audit_rej => ea.audit_rejected += 1,
            RunStatus::Failed => {
                ea.failed += 1;
                ea.wasted_tokens = ea.wasted_tokens.saturating_add(tot);
            }
            RunStatus::TimedOut => {
                ea.timed_out += 1;
                ea.wasted_tokens = ea.wasted_tokens.saturating_add(tot);
            }
            RunStatus::Killed => {
                ea.killed += 1;
                ea.wasted_tokens = ea.wasted_tokens.saturating_add(tot);
            }
            RunStatus::LimitHit => {
                ea.limit_hit += 1;
                ea.wasted_tokens = ea.wasted_tokens.saturating_add(tot);
            }
        }

        let acct_id = r
            .account_id
            .as_deref()
            .unwrap_or("(unassigned)")
            .to_string();
        let friendly = r
            .account_id
            .as_deref()
            .and_then(|id| acct_names.get(id))
            .cloned();
        let aa = by_account
            .entry(acct_id.clone())
            .or_insert_with(|| AccountAgg {
                name: friendly.clone().unwrap_or_else(|| acct_id.clone()),
                ..AccountAgg::default()
            });
        aa.runs += 1;
        aa.tokens_in = aa.tokens_in.saturating_add(r.tokens_in);
        aa.tokens_out = aa.tokens_out.saturating_add(r.tokens_out);
        if wasted {
            aa.wasted_runs += 1;
            aa.wasted_tokens = aa.wasted_tokens.saturating_add(tot);
        }

        let te = extras.entry(r.task_id.clone()).or_default();
        if wasted {
            te.wasted_runs += 1;
            te.wasted_tokens = te.wasted_tokens.saturating_add(tot);
        }
        if audit_rej {
            te.audit_rejected_tokens = te.audit_rejected_tokens.saturating_add(tot);
        }
        if is_audit {
            te.audit_tokens = te.audit_tokens.saturating_add(tot);
        }
        match r.status {
            RunStatus::TimedOut => te.runs_timed_out += 1,
            RunStatus::Killed => te.runs_killed += 1,
            RunStatus::LimitHit => te.runs_limit_hit += 1,
            _ => {}
        }
        if tot == 0 && r.status != RunStatus::Running {
            te.zero_token_runs += 1;
        }

        if tot >= SINGLE_RUN_SPIKE_THRESHOLD {
            if !spike_run_tasks.contains(&r.task_id) {
                spike_run_tasks.push(r.task_id.clone());
            }
            if r.started_at >= spike_since && !spike_run_tasks_1h.contains(&r.task_id) {
                spike_run_tasks_1h.push(r.task_id.clone());
            }
        }
        if tot > 0 || wasted || is_stale_running {
            let (title, rname) = task_map
                .get(r.task_id.as_str())
                .map(|t| (t.title.clone(), repo_basename(&t.repo_root)))
                .unwrap_or_else(|| (r.task_id.clone(), String::new()));
            top_runs.push(TopRunRow {
                run: r,
                task_title: title,
                repo_name: rname,
                account_name: friendly,
                total_tokens: tot,
                duration_secs: dur_secs,
                wasted,
                audit_rejected: audit_rej,
            });
        }
    }

    for t in tasks {
        if let Some(b) = budgets.get(&t.root_id) {
            let te = extras.entry(t.id.clone()).or_default();
            te.budget_used = Some(b.tokens_used);
            te.budget_limit = Some(b.token_limit);
            te.budget_frozen = b.frozen;
        }
    }

    let mut wasted_by_task: Vec<(&String, u64)> = extras
        .iter()
        .filter(|(_, e)| e.wasted_tokens > 0)
        .map(|(id, e)| (id, e.wasted_tokens))
        .collect();
    wasted_by_task.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let top_wasted_tasks: Vec<String> = wasted_by_task
        .into_iter()
        .take(4)
        .map(|(id, _)| id.clone())
        .collect();

    top_runs.sort_by(|a, b| {
        b.total_tokens
            .cmp(&a.total_tokens)
            .then_with(|| b.run.started_at.cmp(&a.run.started_at))
            .then_with(|| a.run.id.cmp(&b.run.id))
    });
    top_runs.truncate(TOP_RUNS_LIMIT);

    let (tokens_last_1h, _) = db.cost_window_usage(spike_since).unwrap_or((0, None));
    let (tokens_last_24h, _) = db
        .cost_window_usage(now.saturating_sub(DAY_SECS))
        .unwrap_or((0, None));
    let daily_cap = dctx.cost_guard.daily_token_cap;
    let raw_paused = db
        .get_meta(META_COST_PAUSED_UNTIL)
        .ok()
        .flatten()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .filter(|&v| v > 0);
    let cost_paused_until = raw_paused.filter(|&v| v > now && daily_cap > 0);
    let burn = BurnSnapshot {
        tokens_last_1h,
        tokens_last_24h,
        daily_cap,
        spike_limit_1h: if daily_cap > 0 && dctx.cost_guard.spike_factor > 0 {
            let raw =
                (u128::from(dctx.cost_guard.spike_factor) * u128::from(daily_cap) / 24).max(1);
            u64::try_from(raw).unwrap_or(u64::MAX)
        } else {
            0
        },
        cost_paused_until,
        cost_paused_stale: raw_paused.is_some() && cost_paused_until.is_none(),
        factory_frozen: db.get_meta(META_FACTORY_FROZEN).ok().flatten().as_deref() == Some("1"),
    };

    let mut budget_rows: Vec<(&RootBudget, String, f64, bool)> = budgets
        .values()
        .filter(|b| b.token_limit > 0 && (b.frozen || ratio_ge(b.tokens_used, b.token_limit, 50)))
        .map(|b| {
            let title = task_map
                .get(b.root_id.as_str())
                .map(|t| t.title.clone())
                .or_else(|| db.get_task(&b.root_id).ok().map(|t| t.title))
                .unwrap_or_else(|| b.root_id.clone());
            (
                b,
                title,
                pct(b.tokens_used, b.token_limit),
                open_roots.contains(&b.root_id),
            )
        })
        .collect();
    budget_rows.sort_by(|a, b| {
        b.3.cmp(&a.3)
            .then_with(|| b.0.frozen.cmp(&a.0.frozen))
            .then_with(|| b.2.total_cmp(&a.2))
            .then_with(|| b.0.tokens_used.cmp(&a.0.tokens_used))
            .then_with(|| a.0.root_id.cmp(&b.0.root_id))
    });

    let total_tokens = totals.total_in.saturating_add(totals.total_out);
    let mut alerts = build_alerts(AlertInputs {
        total_tokens,
        total_runs: totals.total_runs,
        wasted_tok: totals.wasted_tok,
        wasted_runs: totals.wasted_runs,
        wasted_pct: pct(totals.wasted_tok, total_tokens),
        failed_tok: totals.failed_tok,
        timed_out_tok: totals.timed_out_tok,
        timed_out_runs: totals.timed_out_runs,
        killed_tok: totals.killed_tok,
        zero_token_failed_runs: totals.zero_token_failed_runs,
        stale_running_runs: totals.stale_running_runs,
        stale_running_tasks: &stale_running_tasks,
        audit_tok: totals.audit_tok,
        audit_runs: totals.audit_runs,
        audit_pct: pct(totals.audit_tok, total_tokens),
        avg_audit_tokens: totals.audit_tok.checked_div(totals.audit_runs).unwrap_or(0),
        tokens_last_1h: burn.tokens_last_1h,
        tokens_last_24h: burn.tokens_last_24h,
        daily_cap: burn.daily_cap,
        spike_limit_1h: burn.spike_limit_1h,
        cost_paused_until: burn.cost_paused_until,
        max_single_run: top_runs.first().map_or(0, |r| r.total_tokens),
        top_wasted_tasks: &top_wasted_tasks,
        timeout_task_ids: &timeout_task_ids,
        spike_run_tasks: &spike_run_tasks,
        spike_run_tasks_1h: &spike_run_tasks_1h,
        dup_audits: &dup_audits,
        budget_rows: &budget_rows,
    });
    alerts.sort_by(|a, b| {
        b.severity_rank
            .cmp(&a.severity_rank)
            .then_with(|| a.code.cmp(b.code))
    });
    let alerts_truncated = alerts.len() > MAX_ALERTS;
    alerts.truncate(MAX_ALERTS);

    let payload = build_diagnostics_payload(
        &totals,
        &burn,
        dctx,
        &alerts,
        alerts_truncated,
        by_exec,
        by_account,
        top_runs,
        &dup_audits,
        &budget_rows,
    );
    (payload, extras)
}
