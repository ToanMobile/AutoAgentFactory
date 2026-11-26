//! Unified 4-pillar Quality, Speed/Slot, Account-Starvation, and Token-Efficiency
//! diagnostics for `/health` (legacy `/studio`, `GET /api/report`) and `/aaf_heath`.
//!
//! Zero `unsafe`, zero `tokio`, zero `unwrap`/`expect` in non-test code.

mod alerts;
mod probes;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use aaf_domain::account::AccountState;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::run::RunRecord;
use aaf_domain::task::{Task, TaskState};
use aaf_engine::telemetry::FactoryReport;
use serde_json::{Value, json};

use self::alerts::{AlertFacts, collect_ops_alerts};
use self::probes::{detect_worker_issues, inspect_slots_and_orphans, summarize_account_pool};
use super::studio_tokens::is_audit_executor;

const STUCK_GATING_WARN_SECS: u64 = 1200;
const STUCK_GATING_HIGH_SECS: u64 = 2700;
const GATING_SPIN_WINDOW_SECS: i64 = 1800;
const GATING_SPIN_MIN_GATES: u64 = 3;
const STALE_REVIEW_WARN_SECS: u64 = 14_400;
const REWORK_CHURN_MIN_CYCLES: u32 = 4;

pub(super) struct OpsInput<'a> {
    pub data_dir: Option<&'a Path>,
    pub tasks: &'a [Task],
    pub runs_by_task: &'a HashMap<String, Vec<RunRecord>>,
    pub events: &'a [FactoryEvent],
    pub accounts: &'a [AccountState],
    pub blocked_upstream: &'a [(String, String)],
    pub report: &'a FactoryReport,
    pub token_diagnostics: &'a Value,
    pub breakdown: &'a [Value],
    pub since: Option<i64>,
    pub now: i64,
}

pub(super) fn empty_ops_diagnostics() -> Value {
    json!({
        "quality": {
            "mech_avg": null,
            "mech_samples": 0,
            "audit_effective_avg": null,
            "llm_avg": null,
            "audit_samples": 0,
            "clear_with_mech_zero_count": 0,
            "clear_with_mech_zero_tasks": [],
            "vacuous_test_warns": 0,
            "high_rework_churn_tasks": [],
        },
        "speed": {
            "done_1h": 0,
            "done_window": 0,
            "coding_dur_p50_secs": null,
            "coding_dur_p90_secs": null,
            "coding_samples": 0,
            "audit_dur_p50_secs": null,
            "audit_samples": 0,
            "gate_dur_p50_secs": null,
            "gate_dur_p90_secs": null,
            "gate_avg_secs": null,
            "gate_samples": 0,
            "active_slots": 0,
            "draining": false,
            "waiters": 0,
            "oldest_wait_secs": 0,
            "dead_orphan_slots": [],
            "stalled_workers": [],
            "stuck_gating_tasks": [],
            "gating_spin_tasks": [],
            "long_running_workers": [],
            "looping_workers": [],
            "stale_review_tasks": [],
            "dag_bottlenecks": [],
            "account_pool": {
                "by_provider": [],
                "total_accounts": 0,
                "total_available": 0,
                "total_leased": 0,
                "total_cooling": 0,
                "waiting_for_account": 0,
                "waiting_repos": [],
            },
        },
        "efficiency": {
            "tokens_per_done": null,
            "in_out_ratio": null,
            "top_token_tasks": [],
        },
        "alerts": [],
    })
}

fn percentile_u64(vals: &mut [u64], pct: u64) -> Option<u64> {
    if vals.is_empty() {
        return None;
    }
    vals.sort_unstable();
    let span = vals.len().saturating_sub(1);
    let idx = ((u64::try_from(span).unwrap_or(0).saturating_mul(pct) + 50) / 100) as usize;
    vals.get(idx.min(span)).copied()
}

fn mean_round1(vals: &[u64]) -> Option<f64> {
    if vals.is_empty() {
        return None;
    }
    let sum: u64 = vals.iter().copied().fold(0, u64::saturating_add);
    let avg = (sum as f64) / (vals.len() as f64);
    Some((avg * 10.0).round() / 10.0)
}

fn is_aux_non_coding(executor: &str) -> bool {
    executor.eq_ignore_ascii_case("plan") || executor.eq_ignore_ascii_case("split")
}

fn classify_gating_cause(
    acct: &probes::AccountPoolSummary,
    slot_probe: &probes::SlotProbe,
    has_active_run: bool,
) -> &'static str {
    if has_active_run {
        "audit_or_worker_running"
    } else if acct.total_accounts > 0 && acct.total_available == 0 {
        if acct.total_leased > 0 {
            "all_accounts_leased"
        } else {
            "all_accounts_cooling"
        }
    } else if acct.waiting_for_account > 0 {
        "waiting_for_account"
    } else if slot_probe.waiters > 0 || slot_probe.draining {
        "heavy_slot_waiting"
    } else {
        "idle_post_gate_no_run"
    }
}

pub(super) fn collect_ops_diagnostics(input: &OpsInput<'_>) -> Value {
    let since_1h = input.now.saturating_sub(3600);
    let since_spin = input.now.saturating_sub(GATING_SPIN_WINDOW_SECS);
    let slot_probe = inspect_slots_and_orphans(input.data_dir);
    let worker_probe = detect_worker_issues(input.data_dir, input.runs_by_task, input.now);
    let acct_pool = summarize_account_pool(
        input.data_dir,
        input.accounts,
        input.runs_by_task,
        input.now,
    );

    let mut task_meta: HashMap<&str, (TaskState, Option<&str>)> =
        HashMap::with_capacity(input.tasks.len());
    let (mut done_1h, mut done_window, mut done_with_runs) = (0u64, 0u64, 0u64);
    let (mut open_ready_or_gating, mut open_gating_count) = (0usize, 0usize);
    let (mut churn_labels, mut churn_ids) = (Vec::new(), Vec::new());
    let mut gating_ids: BTreeSet<String> = BTreeSet::new();
    let mut stuck_gating_tasks: Vec<Value> = Vec::new();
    let mut stale_review_tasks: Vec<Value> = Vec::new();

    for t in input.tasks {
        task_meta.insert(
            t.id.as_str(),
            (
                t.state,
                t.blocked_reason
                    .as_ref()
                    .map(aaf_domain::blocked::BlockedReason::kind),
            ),
        );
        if t.state == TaskState::Done {
            if t.updated_at >= since_1h {
                done_1h += 1;
            }
            if input.since.is_none_or(|c| t.updated_at >= c) {
                done_window += 1;
                if input
                    .runs_by_task
                    .get(&t.id)
                    .is_some_and(|rs| !rs.is_empty())
                {
                    done_with_runs += 1;
                }
            }
        }
        if matches!(t.state, TaskState::Ready | TaskState::Gating) {
            open_ready_or_gating += 1;
        }
        if t.state == TaskState::Gating {
            open_gating_count += 1;
            gating_ids.insert(t.id.clone());
            let age = u64::try_from(input.now.saturating_sub(t.updated_at).max(0)).unwrap_or(0);
            if age >= STUCK_GATING_WARN_SECS {
                let sev = if age >= STUCK_GATING_HIGH_SECS {
                    "high"
                } else {
                    "medium"
                };
                let has_run = input.runs_by_task.get(&t.id).is_some_and(|rs| {
                    rs.iter()
                        .any(|r| r.status == aaf_domain::run::RunStatus::Running)
                });
                let cause = classify_gating_cause(&acct_pool, &slot_probe, has_run);
                stuck_gating_tasks.push(json!({
                    "task_id": t.id,
                    "age_secs": age,
                    "severity": sev,
                    "cause": cause,
                }));
            }
        }
        if t.state == TaskState::Review {
            let age = u64::try_from(input.now.saturating_sub(t.updated_at).max(0)).unwrap_or(0);
            if age >= STALE_REVIEW_WARN_SECS {
                stale_review_tasks.push(json!({
                    "task_id": t.id,
                    "age_secs": age,
                }));
            }
        }
        if matches!(t.state, TaskState::Running | TaskState::Gating)
            && t.unblock_cycles >= REWORK_CHURN_MIN_CYCLES
        {
            churn_labels.push(format!("{} (cyc={})", t.id, t.unblock_cycles));
            churn_ids.push(t.id.clone());
        }
    }
    stuck_gating_tasks.sort_by(|a, b| {
        b["age_secs"]
            .as_u64()
            .unwrap_or(0)
            .cmp(&a["age_secs"].as_u64().unwrap_or(0))
    });
    stale_review_tasks.sort_by(|a, b| {
        b["age_secs"]
            .as_u64()
            .unwrap_or(0)
            .cmp(&a["age_secs"].as_u64().unwrap_or(0))
    });

    let mut blocker_groups: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, blocker) in input.blocked_upstream {
        *blocker_groups.entry(blocker.as_str()).or_insert(0) += 1;
    }
    let mut dag_bottlenecks: Vec<Value> = blocker_groups
        .into_iter()
        .filter(|(_, count)| *count >= 2)
        .map(|(blocker, count)| {
            let (st, reason) = task_meta
                .get(blocker)
                .copied()
                .map_or(("unknown", None), |(s, r)| (s.as_str(), r));
            json!({
                "blocker_id": blocker,
                "blocker_state": st,
                "blocker_reason": reason,
                "blocked_count": count,
            })
        })
        .collect();
    dag_bottlenecks.sort_by(|a, b| {
        b["blocked_count"]
            .as_u64()
            .unwrap_or(0)
            .cmp(&a["blocked_count"].as_u64().unwrap_or(0))
    });

    let (mut code_durs, mut audit_durs) = (Vec::new(), Vec::new());
    for runs in input.runs_by_task.values() {
        for r in runs {
            let Some(ended) = r.ended_at else { continue };
            let dur = u64::try_from(ended.saturating_sub(r.started_at).max(0)).unwrap_or(0);
            if is_audit_executor(&r.executor) {
                audit_durs.push(dur);
            } else if !is_aux_non_coding(&r.executor) {
                code_durs.push(dur);
            }
        }
    }

    let (mut gate_durs, mut mech_scores, mut eff_scores, mut llm_avgs) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut clear_zero_tasks: BTreeSet<String> = BTreeSet::new();
    let mut recent_gates_by_task: HashMap<String, u64> = HashMap::new();
    let (mut clear_zero_count, mut vacuous_warns) = (0u64, 0u64);
    let (mut clear_zero_last_at, mut vacuous_last_at): (Option<i64>, Option<i64>) = (None, None);

    for ev in input.events {
        let Ok(payload) = serde_json::from_str::<Value>(&ev.payload_json) else {
            continue;
        };
        match ev.kind {
            EventKind::GateResult => {
                if ev.created_at >= since_spin
                    && let Some(tid) = &ev.task_id
                    && gating_ids.contains(tid)
                {
                    *recent_gates_by_task.entry(tid.clone()).or_insert(0) += 1;
                }
                if let Some(ms) = payload.get("duration_ms").and_then(Value::as_u64) {
                    gate_durs.push((ms + 500) / 1000);
                }
                if let Some(score) = payload.get("mechanical_score").and_then(Value::as_u64) {
                    mech_scores.push(score);
                    let is_clear = payload
                        .get("status")
                        .and_then(Value::as_str)
                        .is_some_and(|s| s.eq_ignore_ascii_case("clear"));
                    if is_clear && score == 0 {
                        clear_zero_count += 1;
                        clear_zero_last_at = Some(
                            clear_zero_last_at.map_or(ev.created_at, |p| p.max(ev.created_at)),
                        );
                        if let Some(tid) = &ev.task_id {
                            clear_zero_tasks.insert(tid.clone());
                        }
                    }
                }
                if payload
                    .get("reasons")
                    .and_then(Value::as_array)
                    .is_some_and(|arr| {
                        arr.iter().any(|r| {
                            r.as_str()
                                .is_some_and(|s| s.to_ascii_lowercase().contains("vacuous test"))
                        })
                    })
                {
                    vacuous_warns += 1;
                    vacuous_last_at =
                        Some(vacuous_last_at.map_or(ev.created_at, |p| p.max(ev.created_at)));
                }
            }
            EventKind::CrossAudit => {
                let skip = payload.get("short_circuited").and_then(Value::as_bool) == Some(true)
                    || payload.get("retry").and_then(Value::as_bool) == Some(true)
                    || payload.get("reused_from").is_some_and(|v| !v.is_null());
                if skip {
                    continue;
                }
                if let Some(eff) = payload.get("effective_score").and_then(Value::as_u64) {
                    eff_scores.push(eff);
                }
                if let Some(llm) = payload.get("llm_average").and_then(Value::as_u64) {
                    llm_avgs.push(llm);
                }
            }
            _ => {}
        }
    }

    let mut gating_spin_tasks: Vec<Value> = recent_gates_by_task
        .into_iter()
        .filter(|(_, count)| *count >= GATING_SPIN_MIN_GATES)
        .map(|(tid, count)| json!({ "task_id": tid, "gate_count_30m": count }))
        .collect();
    gating_spin_tasks.sort_by(|a, b| {
        b["gate_count_30m"]
            .as_u64()
            .unwrap_or(0)
            .cmp(&a["gate_count_30m"].as_u64().unwrap_or(0))
    });

    let td_sum = input.token_diagnostics.get("summary");
    let total_tokens = td_sum
        .and_then(|s| s.get("total_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let tokens_in = td_sum
        .and_then(|s| s.get("tokens_in"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let tokens_out = td_sum
        .and_then(|s| s.get("tokens_out"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    // Use `done_with_runs` so archived historical done tasks with zero runs in the
    // current window (e.g., after `/aaf-studio-reset`) never dilute tokens_per_done.
    let tpd_denom = if done_with_runs > 0 {
        done_with_runs
    } else {
        done_window
    };
    let tokens_per_done = (tpd_denom > 0 && total_tokens > 0).then(|| total_tokens / tpd_denom);
    let in_out_ratio =
        (tokens_out > 0).then(|| ((tokens_in as f64 / tokens_out as f64) * 10.0).round() / 10.0);

    let mut top_tasks: Vec<&Value> = input
        .breakdown
        .iter()
        .filter(|t| t.get("tokens_total").and_then(Value::as_u64).unwrap_or(0) > 0)
        .collect();
    top_tasks.sort_by(|a, b| {
        b.get("tokens_total")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .cmp(&a.get("tokens_total").and_then(Value::as_u64).unwrap_or(0))
    });
    let top_token_tasks: Vec<Value> = top_tasks
        .into_iter()
        .take(3)
        .map(|t| {
            json!({
                "id": t.get("id").and_then(Value::as_str).unwrap_or(""),
                "title": t.get("title").and_then(Value::as_str).unwrap_or(""),
                "state": t.get("state").and_then(Value::as_str).unwrap_or(""),
                "tokens_total": t.get("tokens_total").and_then(Value::as_u64).unwrap_or(0),
                "wasted_tokens": t.get("wasted_tokens").and_then(Value::as_u64).unwrap_or(0),
                "runs_total": t.get("runs_total").and_then(Value::as_u64).unwrap_or(0),
            })
        })
        .collect();

    let gate_avg_secs = input
        .report
        .autonomy
        .gate_avg_duration_ms
        .map(|ms| (ms + 500) / 1000);
    let coding_p50 = percentile_u64(&mut code_durs, 50);
    let coding_p90 = percentile_u64(&mut code_durs, 90);
    let audit_p50 = percentile_u64(&mut audit_durs, 50);
    let gate_p50 = percentile_u64(&mut gate_durs, 50);
    let gate_p90 = percentile_u64(&mut gate_durs, 90);

    let clear_zero_vec: Vec<String> = clear_zero_tasks.into_iter().collect();
    let alerts = collect_ops_alerts(&AlertFacts {
        now: input.now,
        autonomy: &input.report.autonomy,
        clear_zero_count,
        clear_zero_tasks: &clear_zero_vec,
        clear_zero_last_at,
        vacuous_warns,
        vacuous_last_at,
        churn_labels: &churn_labels,
        churn_ids: &churn_ids,
        dead_orphan_slots: &slot_probe.dead_orphan_slots,
        draining: slot_probe.draining,
        active_slots: slot_probe.active_slots,
        waiters: slot_probe.waiters,
        oldest_wait_secs: slot_probe.oldest_wait_secs,
        stalled_workers: &worker_probe.stalled_workers,
        stuck_gating_tasks: &stuck_gating_tasks,
        gating_spin_tasks: &gating_spin_tasks,
        long_running_workers: &worker_probe.long_running_workers,
        looping_workers: &worker_probe.looping_workers,
        stale_review_tasks: &stale_review_tasks,
        dag_bottlenecks: &dag_bottlenecks,
        account_pool: &acct_pool,
        open_ready_or_gating,
        open_gating_count,
        gate_avg_secs,
        done_window,
        total_tokens,
        tokens_per_done,
    });

    json!({
        "quality": {
            "mech_avg": mean_round1(&mech_scores),
            "mech_samples": mech_scores.len(),
            "audit_effective_avg": mean_round1(&eff_scores),
            "llm_avg": mean_round1(&llm_avgs),
            "audit_samples": llm_avgs.len(),
            "clear_with_mech_zero_count": clear_zero_count,
            "clear_with_mech_zero_tasks": clear_zero_vec,
            "vacuous_test_warns": vacuous_warns,
            "high_rework_churn_tasks": churn_labels,
        },
        "speed": {
            "done_1h": done_1h,
            "done_window": done_window,
            "coding_dur_p50_secs": coding_p50,
            "coding_dur_p90_secs": coding_p90,
            "coding_samples": code_durs.len(),
            "audit_dur_p50_secs": audit_p50,
            "audit_samples": audit_durs.len(),
            "gate_dur_p50_secs": gate_p50,
            "gate_dur_p90_secs": gate_p90,
            "gate_avg_secs": gate_avg_secs,
            "gate_samples": gate_durs.len(),
            "active_slots": slot_probe.active_slots,
            "draining": slot_probe.draining,
            "waiters": slot_probe.waiters,
            "oldest_wait_secs": slot_probe.oldest_wait_secs,
            "dead_orphan_slots": slot_probe.dead_orphan_slots,
            "stalled_workers": worker_probe.stalled_workers,
            "stuck_gating_tasks": stuck_gating_tasks,
            "gating_spin_tasks": gating_spin_tasks,
            "long_running_workers": worker_probe.long_running_workers,
            "looping_workers": worker_probe.looping_workers,
            "stale_review_tasks": stale_review_tasks,
            "dag_bottlenecks": dag_bottlenecks,
            "account_pool": {
                "by_provider": acct_pool.by_provider,
                "total_accounts": acct_pool.total_accounts,
                "total_available": acct_pool.total_available,
                "total_leased": acct_pool.total_leased,
                "total_cooling": acct_pool.total_cooling,
                "waiting_for_account": acct_pool.waiting_for_account,
                "waiting_repos": acct_pool.waiting_repos,
            },
        },
        "efficiency": {
            "tokens_per_done": tokens_per_done,
            "in_out_ratio": in_out_ratio,
            "top_token_tasks": top_token_tasks,
        },
        "alerts": alerts,
    })
}
