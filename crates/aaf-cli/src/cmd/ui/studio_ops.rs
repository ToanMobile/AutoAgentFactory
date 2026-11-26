//! Unified 4-pillar Quality, Speed/Slot, and Token-Efficiency diagnostics for
//! `/studio` (`GET /api/report`) and `/aaf_heath` (Single Source of Truth).
//!
//! Zero `unsafe`, zero `tokio`, zero `unwrap`/`expect` in non-test code.

mod alerts;
#[cfg(test)]
mod tests;

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::run::{RunRecord, RunStatus};
use aaf_domain::task::{Task, TaskState};
use aaf_engine::executors::reaper::validate_run_id;
use aaf_engine::telemetry::{FactoryReport, scan_throttle};
use aaf_engine::throttle::{GroupControl, SystemGroup};
use serde_json::{Value, json};

use self::alerts::{AlertFacts, collect_ops_alerts};
use super::studio_tokens::is_audit_executor;

const STALL_WARN_SECS: u64 = 480;
const STALL_HIGH_SECS: u64 = 900;
const STALL_NO_STREAM_SECS: u64 = 300;
const REWORK_CHURN_MIN_CYCLES: u32 = 4;

pub(super) struct OpsInput<'a> {
    pub data_dir: Option<&'a Path>,
    pub tasks: &'a [Task],
    pub runs_by_task: &'a HashMap<String, Vec<RunRecord>>,
    pub events: &'a [FactoryEvent],
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
            "drain_age_secs": 0,
            "waiters": 0,
            "oldest_wait_secs": 0,
            "dead_orphan_slots": [],
            "stalled_workers": [],
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

fn parse_pgid(text: &str) -> Option<u32> {
    text.lines().find_map(|ln| {
        ln.trim()
            .strip_prefix("pgid=")
            .and_then(|v| v.trim().parse::<u32>().ok())
    })
}

struct SlotProbe {
    active_slots: usize,
    draining: bool,
    drain_age_secs: u64,
    waiters: usize,
    oldest_wait_secs: u64,
    dead_orphan_slots: Vec<String>,
}

fn inspect_slots_and_orphans(data_dir: Option<&Path>, now: i64) -> SlotProbe {
    let Some(dir) = data_dir else {
        return SlotProbe {
            active_slots: 0,
            draining: false,
            drain_age_secs: 0,
            waiters: 0,
            oldest_wait_secs: 0,
            dead_orphan_slots: Vec::new(),
        };
    };
    let (snap, entries) = scan_throttle(dir);
    let mut dead_orphans = Vec::new();
    for entry in entries {
        if entry.locked || !fs::metadata(&entry.path).is_ok_and(|m| m.len() > 0) {
            continue;
        }
        let text = fs::read_to_string(&entry.path).unwrap_or_default();
        let pgid = parse_pgid(&text);
        let is_dead = match pgid {
            Some(id) => matches!(SystemGroup.read_group_state(id), Ok(None)),
            None => true,
        };
        if is_dead {
            let label = match pgid {
                Some(id) => format!("heavy.{}.lock(pgid={id})", entry.slot),
                None => format!("heavy.{}.lock(pgid=none)", entry.slot),
            };
            dead_orphans.push(label);
        }
    }
    let now_time = UNIX_EPOCH + Duration::from_secs(u64::try_from(now.max(0)).unwrap_or(0));
    let drain_age_secs = if snap.drain_lock_present {
        fs::metadata(dir.join("throttle").join("drain.lock"))
            .and_then(|m| m.modified())
            .ok()
            .and_then(|mt| now_time.duration_since(mt).ok())
            .map_or(0, |d| d.as_secs())
    } else {
        0
    };
    SlotProbe {
        active_slots: snap.throttle_slots,
        draining: snap.drain_lock_present,
        drain_age_secs,
        waiters: snap.waiters,
        oldest_wait_secs: snap.oldest_wait_secs.unwrap_or(0),
        dead_orphan_slots: dead_orphans,
    }
}

fn detect_stalled_workers(
    data_dir: Option<&Path>,
    runs_by_task: &HashMap<String, Vec<RunRecord>>,
    now: i64,
) -> Vec<Value> {
    let Some(dir) = data_dir else {
        return Vec::new();
    };
    let streams_dir = dir.join("streams");
    let now_time = UNIX_EPOCH + Duration::from_secs(u64::try_from(now.max(0)).unwrap_or(0));
    let mut stalled = Vec::new();
    for runs in runs_by_task.values() {
        for r in runs.iter().filter(|r| r.status == RunStatus::Running) {
            if validate_run_id(&r.id).is_err() {
                continue;
            }
            let stream_path = streams_dir.join(format!("{}.jsonl", r.id));
            if let Ok(meta) = fs::metadata(&stream_path)
                && let Ok(mtime) = meta.modified()
            {
                let idle = now_time
                    .duration_since(mtime)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                if idle >= STALL_WARN_SECS {
                    let sev = if idle >= STALL_HIGH_SECS {
                        "high"
                    } else {
                        "medium"
                    };
                    stalled.push(json!({
                        "task_id": r.task_id,
                        "run_id": r.id,
                        "idle_secs": idle,
                        "severity": sev,
                    }));
                }
            } else {
                let age = u64::try_from(now.saturating_sub(r.started_at).max(0)).unwrap_or(0);
                if age >= STALL_NO_STREAM_SECS {
                    let sev = if age >= STALL_HIGH_SECS {
                        "high"
                    } else {
                        "medium"
                    };
                    stalled.push(json!({
                        "task_id": r.task_id,
                        "run_id": r.id,
                        "idle_secs": age,
                        "severity": sev,
                    }));
                }
            }
        }
    }
    stalled.sort_by(|a, b| {
        b["idle_secs"]
            .as_u64()
            .unwrap_or(0)
            .cmp(&a["idle_secs"].as_u64().unwrap_or(0))
    });
    stalled
}

pub(super) fn collect_ops_diagnostics(input: &OpsInput<'_>) -> Value {
    let since_1h = input.now.saturating_sub(3600);
    let (mut done_1h, mut done_window) = (0u64, 0u64);
    let (mut churn_labels, mut churn_ids) = (Vec::new(), Vec::new());
    for t in input.tasks {
        if t.state == TaskState::Done {
            if t.updated_at >= since_1h {
                done_1h += 1;
            }
            if input.since.is_none_or(|c| t.updated_at >= c) {
                done_window += 1;
            }
        }
        if matches!(t.state, TaskState::Running | TaskState::Gating)
            && t.unblock_cycles >= REWORK_CHURN_MIN_CYCLES
        {
            churn_labels.push(format!("{} (cyc={})", t.id, t.unblock_cycles));
            churn_ids.push(t.id.clone());
        }
    }

    let (mut code_durs, mut audit_durs) = (Vec::new(), Vec::new());
    for runs in input.runs_by_task.values() {
        for r in runs {
            let Some(ended) = r.ended_at else { continue };
            let dur = u64::try_from(ended.saturating_sub(r.started_at).max(0)).unwrap_or(0);
            if is_audit_executor(&r.executor) {
                audit_durs.push(dur);
            } else if !r.executor.eq_ignore_ascii_case("plan") {
                code_durs.push(dur);
            }
        }
    }

    let (mut gate_durs, mut mech_scores, mut eff_scores, mut llm_avgs) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut clear_zero_tasks: BTreeSet<String> = BTreeSet::new();
    let (mut clear_zero_count, mut vacuous_warns) = (0u64, 0u64);
    let (mut clear_zero_last_at, mut vacuous_last_at): (Option<i64>, Option<i64>) = (None, None);

    for ev in input.events {
        let Ok(payload) = serde_json::from_str::<Value>(&ev.payload_json) else {
            continue;
        };
        match ev.kind {
            EventKind::GateResult => {
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

    let slot_probe = inspect_slots_and_orphans(input.data_dir, input.now);
    let stalled_workers = detect_stalled_workers(input.data_dir, input.runs_by_task, input.now);

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
    let tokens_per_done = (done_window > 0 && total_tokens > 0).then(|| total_tokens / done_window);
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
        drain_age_secs: slot_probe.drain_age_secs,
        active_slots: slot_probe.active_slots,
        waiters: slot_probe.waiters,
        oldest_wait_secs: slot_probe.oldest_wait_secs,
        stalled_workers: &stalled_workers,
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
            "drain_age_secs": slot_probe.drain_age_secs,
            "waiters": slot_probe.waiters,
            "oldest_wait_secs": slot_probe.oldest_wait_secs,
            "dead_orphan_slots": slot_probe.dead_orphan_slots,
            "stalled_workers": stalled_workers,
        },
        "efficiency": {
            "tokens_per_done": tokens_per_done,
            "in_out_ratio": in_out_ratio,
            "top_token_tasks": top_token_tasks,
        },
        "alerts": alerts,
    })
}
