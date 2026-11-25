//! Read-only autonomy, first-pass gate, CrossAudit, and escalation telemetry (RFC-054).

use std::collections::{HashMap, HashSet};

use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{PortResult, TaskStore};
use aaf_domain::task::TaskState;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::db::Db;

/// Autonomous execution, first-pass gate, CrossAudit, and escalation KPIs.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AutonomyMetrics {
    #[serde(default)]
    pub done_tasks: u64,
    #[serde(default)]
    pub done_without_human: u64,
    #[serde(default)]
    pub autonomy_rate_pct: Option<f64>,
    #[serde(default)]
    pub gate_tasks_evaluated: u64,
    #[serde(default)]
    pub gate_first_pass_clear: u64,
    #[serde(default)]
    pub gate_first_pass_rate_pct: Option<f64>,
    #[serde(default)]
    pub gate_avg_duration_ms: Option<u64>,
    #[serde(default)]
    pub cross_audit_total: u64,
    #[serde(default)]
    pub cross_audit_failed: u64,
    #[serde(default)]
    pub cross_audit_fail_rate_pct: Option<f64>,
    #[serde(default)]
    pub implementer_escalations: u64,
}

fn rate_pct(numerator: u64, denominator: u64) -> Option<f64> {
    if denominator == 0 {
        return None;
    }
    let raw = (numerator as f64 * 100.0) / denominator as f64;
    Some((raw * 100.0).round() / 100.0)
}

/// Collects autonomy and gate metrics read-only from [`Db`] for `repo_root` (or all repos).
pub fn collect_autonomy_metrics(db: &Db, repo_root: Option<&str>) -> PortResult<AutonomyMetrics> {
    collect_autonomy_metrics_with_done_hint(db, repo_root, true)
}

pub(crate) fn collect_autonomy_metrics_with_done_hint(
    db: &Db,
    repo_root: Option<&str>,
    has_done: bool,
) -> PortResult<AutonomyMetrics> {
    let done_ids: HashSet<String> = if has_done {
        db.list_tasks(repo_root, Some(TaskState::Done))?
            .into_iter()
            .map(|t| t.id)
            .collect()
    } else {
        HashSet::new()
    };
    let events = db.recent_events(repo_root, u32::MAX)?;
    Ok(compute_autonomy_metrics(&done_ids, &events))
}

/// Collects autonomy and gate metrics scoped to an explicit task subset (e.g. `handover --since`).
pub fn collect_autonomy_metrics_for_tasks(
    db: &Db,
    repo_root: Option<&str>,
    tasks: &[(String, TaskState)],
) -> PortResult<AutonomyMetrics> {
    if tasks.is_empty() {
        return Ok(AutonomyMetrics::default());
    }
    let scope_ids: HashSet<&str> = tasks.iter().map(|(id, _)| id.as_str()).collect();
    let done_ids: HashSet<String> = tasks
        .iter()
        .filter(|(_, state)| *state == TaskState::Done)
        .map(|(id, _)| id.clone())
        .collect();
    let events: Vec<FactoryEvent> = db
        .recent_events(repo_root, u32::MAX)?
        .into_iter()
        .filter(|ev| {
            ev.task_id
                .as_deref()
                .is_some_and(|tid| scope_ids.contains(tid))
        })
        .collect();
    Ok(compute_autonomy_metrics(&done_ids, &events))
}

/// Pure aggregation over `done_task_ids` and `events`.
///
/// Only `Done` tasks that still have at least one event in `events` are counted
/// in `done_tasks` / `done_without_human` so tasks whose history was archived by
/// `aaf gc` are excluded from the denominator instead of inflating autonomy.
pub fn compute_autonomy_metrics(
    done_task_ids: &HashSet<String>,
    events: &[FactoryEvent],
) -> AutonomyMetrics {
    let mut observable_done: HashSet<&str> = HashSet::new();
    let mut human_intervened: HashSet<&str> = HashSet::new();
    let mut first_gate: HashMap<&str, ((bool, i64, usize), bool)> = HashMap::new();
    let (mut gate_dur_sum, mut gate_dur_count) = (0u128, 0u64);
    let (mut cross_audit_total, mut cross_audit_failed) = (0u64, 0u64);
    let mut implementer_escalations = 0u64;

    for (idx, ev) in events.iter().enumerate() {
        if let Some(tid) = ev.task_id.as_deref()
            && done_task_ids.contains(tid)
        {
            observable_done.insert(tid);
            if is_human_intervention_event(ev.kind, &ev.payload_json) {
                human_intervened.insert(tid);
            }
        }
        match ev.kind {
            EventKind::GateResult => {
                if let Some(ms) = gate_duration_ms(&ev.payload_json) {
                    gate_dur_sum = gate_dur_sum.saturating_add(u128::from(ms));
                    gate_dur_count = gate_dur_count.saturating_add(1);
                }
                if let Some(tid) = ev.task_id.as_deref() {
                    let key = (ev.id.is_none(), ev.id.unwrap_or(ev.created_at), idx);
                    let clear = is_gate_clear(&ev.payload_json);
                    first_gate
                        .entry(tid)
                        .and_modify(|(best_key, best_clear)| {
                            if key < *best_key {
                                *best_key = key;
                                *best_clear = clear;
                            }
                        })
                        .or_insert((key, clear));
                }
            }
            EventKind::CrossAudit => {
                if let Some(failed) = classify_cross_audit(&ev.payload_json) {
                    cross_audit_total = cross_audit_total.saturating_add(1);
                    if failed {
                        cross_audit_failed = cross_audit_failed.saturating_add(1);
                    }
                }
            }
            EventKind::ImplementerEscalated => {
                implementer_escalations = implementer_escalations.saturating_add(1);
            }
            _ => {}
        }
    }

    let done_tasks = u64::try_from(observable_done.len()).unwrap_or(u64::MAX);
    let intervened = u64::try_from(human_intervened.len()).unwrap_or(u64::MAX);
    let done_without_human = done_tasks.saturating_sub(intervened);
    let gate_tasks_evaluated = u64::try_from(first_gate.len()).unwrap_or(u64::MAX);
    let gate_first_pass_clear =
        u64::try_from(first_gate.values().filter(|(_, clear)| *clear).count()).unwrap_or(u64::MAX);
    let gate_avg_duration_ms = (gate_dur_count > 0)
        .then(|| u64::try_from(gate_dur_sum / u128::from(gate_dur_count)).unwrap_or(u64::MAX));

    AutonomyMetrics {
        done_tasks,
        done_without_human,
        autonomy_rate_pct: rate_pct(done_without_human, done_tasks),
        gate_tasks_evaluated,
        gate_first_pass_clear,
        gate_first_pass_rate_pct: rate_pct(gate_first_pass_clear, gate_tasks_evaluated),
        gate_avg_duration_ms,
        cross_audit_total,
        cross_audit_failed,
        cross_audit_fail_rate_pct: rate_pct(cross_audit_failed, cross_audit_total),
        implementer_escalations,
    }
}

/// Returns `true` iff the event represents human intervention on a task.
///
/// `EventKind::UnblockStep` is automatic factory self-healing and ALWAYS returns `false`.
pub(crate) fn is_human_intervention_event(kind: EventKind, payload_json: &str) -> bool {
    match kind {
        EventKind::ReworkRequested => true,
        EventKind::TaskStateChanged => is_human_state_change(payload_json),
        _ => false,
    }
}

fn is_human_state_change(payload_json: &str) -> bool {
    let Ok(p) = serde_json::from_str::<Value>(payload_json) else {
        return false;
    };
    let from = p["from"].as_str();
    let to = p["to"].as_str();
    let why = p["why"].as_str().unwrap_or("");
    // Standard lifecycle boundaries (`aaf board ready`, `aaf merge`) are not recovery interventions.
    if from == Some("backlog") && to == Some("ready") {
        return false;
    }
    if from == Some("review") && to == Some("done") && why.starts_with("merged PR") {
        return false;
    }
    let actor_is_human = |field: &str| {
        p[field].as_str().is_some_and(|actor| {
            let trimmed = actor.trim();
            !trimmed.is_empty()
                && !trimmed.eq_ignore_ascii_case("factory")
                && !trimmed.eq_ignore_ascii_case("system")
                && !trimmed.eq_ignore_ascii_case("auto")
        })
    };
    if actor_is_human("by") || actor_is_human("source") {
        return true;
    }
    let why_is_human = matches!(why, "undo" | "plan_approved" | "unblocked" | "steer")
        || why.ends_with(" unblock")
        || why.contains(" unblock ")
        || why.ends_with(" unblock-all")
        || why.starts_with("steer:");
    let reason_is_human = p["reason"].as_str().is_some_and(|r| {
        matches!(
            r.trim().to_ascii_lowercase().as_str(),
            "undo" | "unblock" | "unblocked" | "plan_approved" | "steer" | "rework" | "human"
        )
    });
    why_is_human || reason_is_human
}

fn is_gate_clear(payload_json: &str) -> bool {
    serde_json::from_str::<Value>(payload_json)
        .ok()
        .and_then(|p| {
            p["status"]
                .as_str()
                .map(|s| s.eq_ignore_ascii_case("clear"))
        })
        .unwrap_or(false)
}

fn gate_duration_ms(payload_json: &str) -> Option<u64> {
    serde_json::from_str::<Value>(payload_json)
        .ok()
        .and_then(|p| p["duration_ms"].as_u64())
}

/// Returns `None` for non-verdict fallback markers, `Some(true)` for fail, `Some(false)` otherwise.
fn classify_cross_audit(payload_json: &str) -> Option<bool> {
    let Ok(p) = serde_json::from_str::<Value>(payload_json) else {
        return Some(false);
    };
    let verdict = p["verdict"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if verdict == "auth_fallback" {
        return None;
    }
    let is_fail = verdict == "fail"
        || verdict == "failed"
        || verdict.starts_with("fail:")
        || verdict.starts_with("fail ");
    Some(is_fail)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unblock_step_is_never_counted_as_human_intervention() {
        assert!(!is_human_intervention_event(
            EventKind::UnblockStep,
            &json!({"tier": "T1", "by": "human"}).to_string()
        ));
        assert!(is_human_intervention_event(
            EventKind::ReworkRequested,
            &json!({"reason": "fix"}).to_string()
        ));
        assert!(is_human_intervention_event(
            EventKind::TaskStateChanged,
            &json!({"to": "ready", "why": "undo"}).to_string()
        ));
        assert!(is_human_intervention_event(
            EventKind::TaskStateChanged,
            &json!({"to": "ready", "source": "human"}).to_string()
        ));
        assert!(!is_human_intervention_event(
            EventKind::TaskStateChanged,
            &json!({"to": "ready", "why": "auto_retry_due"}).to_string()
        ));
        assert!(!is_human_intervention_event(
            EventKind::TaskStateChanged,
            &json!({"to": "ready", "why": "audit_rework"}).to_string()
        ));
    }

    #[test]
    fn empty_events_yield_none_rates_and_none_avg_duration() {
        let metrics = compute_autonomy_metrics(&HashSet::new(), &[]);
        assert_eq!(metrics, AutonomyMetrics::default());
        assert_eq!(metrics.autonomy_rate_pct, None);
        assert_eq!(metrics.gate_first_pass_rate_pct, None);
        assert_eq!(metrics.gate_avg_duration_ms, None);
        assert_eq!(metrics.cross_audit_fail_rate_pct, None);
    }

    #[test]
    fn cross_audit_skips_auth_fallback_and_handles_fail_variants() {
        assert_eq!(classify_cross_audit(r#"{"verdict":"auth_fallback"}"#), None);
        assert_eq!(classify_cross_audit(r#"{"verdict":"pass"}"#), Some(false));
        assert_eq!(classify_cross_audit(r#"{"verdict":"fail"}"#), Some(true));
        assert_eq!(
            classify_cross_audit(r#"{"verdict":"FAIL: missing test"}"#),
            Some(true)
        );
        assert_eq!(
            classify_cross_audit(r#"{"verdict":"inconclusive: timeout"}"#),
            Some(false)
        );
    }
}
