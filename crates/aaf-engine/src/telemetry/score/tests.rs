use super::*;
use crate::telemetry::{AutonomyMetrics, CostBasis, DurationStats, ReportWindow, TaskCounts};

fn empty_report() -> FactoryReport {
    FactoryReport {
        window: ReportWindow {
            since: None,
            now: 1_000,
        },
        counts: TaskCounts::default(),
        tasks_done: 0,
        lead_time: DurationStats::default(),
        stages: Vec::new(),
        executors: Vec::new(),
        total_tokens_in: 0,
        total_tokens_out: 0,
        estimated_cost_usd: None,
        cost_basis: CostBasis::None,
        top_blocked: Vec::new(),
        autonomy: AutonomyMetrics::default(),
    }
}

fn stage(name: &str, total_secs: u64) -> StageStat {
    StageStat {
        stage: name.into(),
        durations: DurationStats {
            samples: 1,
            total_secs,
            p50_secs: total_secs,
            p90_secs: total_secs,
            max_secs: total_secs,
        },
        open_now: 0,
    }
}

#[test]
fn empty_report_returns_insufficient_data() {
    let res = compute_factory_score(&empty_report());
    assert_eq!(res.status, "insufficient_data");
    assert_eq!(res.score, None);
    assert_eq!(res.grade, None);
    assert_eq!(res.evaluated_tasks, 0);
    assert!(res.bottlenecks.is_empty());
    assert_eq!(res.pillars.len(), 5);
    assert!(res.pillars.iter().all(|p| !p.active && p.score == 0.0));
}

#[test]
fn perfect_report_all_pillars_active_scores_grade_a_without_bottlenecks() {
    let mut r = empty_report();
    r.tasks_done = 10;
    r.counts.done = 10;
    r.stages = vec![
        stage("queued", 10),
        stage("running", 300),
        stage("gating", 60),
    ];
    r.autonomy = AutonomyMetrics {
        done_tasks: 10,
        done_without_human: 10,
        autonomy_rate_pct: Some(100.0),
        gate_tasks_evaluated: 10,
        gate_first_pass_clear: 10,
        gate_first_pass_rate_pct: Some(100.0),
        gate_avg_duration_ms: Some(15_000),
        cross_audit_total: 10,
        cross_audit_failed: 0,
        cross_audit_fail_rate_pct: Some(0.0),
        implementer_escalations: 0,
    };

    let res = compute_factory_score(&r);
    assert_eq!(res.status, "ok");
    assert!(matches!(res.score, Some(95..=100)));
    assert_eq!(res.grade.as_deref(), Some("A"));
    assert_eq!(res.evaluated_tasks, 10);
    assert!(res.pillars.iter().all(|p| p.active));
    assert!(res.bottlenecks.is_empty());
    let wsum: f64 = res.pillars.iter().map(|p| p.weight_pct).sum();
    assert!((wsum - 100.0).abs() < 0.01);
}

#[test]
fn missing_cross_audit_renormalizes_weights_to_100() {
    let mut r = empty_report();
    r.tasks_done = 8;
    r.counts.done = 8;
    r.stages = vec![stage("running", 240), stage("gating", 60)];
    r.autonomy = AutonomyMetrics {
        done_tasks: 8,
        done_without_human: 8,
        autonomy_rate_pct: Some(100.0),
        gate_tasks_evaluated: 8,
        gate_first_pass_clear: 8,
        gate_first_pass_rate_pct: Some(100.0),
        ..AutonomyMetrics::default()
    };

    let res = compute_factory_score(&r);
    assert_eq!(res.status, "ok");
    assert_eq!(res.score, Some(100));
    assert_eq!(res.grade.as_deref(), Some("A"));
    let audit = res
        .pillars
        .iter()
        .find(|p| p.key == "audit_integrity")
        .unwrap();
    assert!(!audit.active);
    assert_eq!(audit.weight_pct, 0.0);
    assert_eq!(audit.max_score, 0.0);
    let active_w: f64 = res
        .pillars
        .iter()
        .filter(|p| p.active)
        .map(|p| p.weight_pct)
        .sum();
    assert!((active_w - 100.0).abs() < 0.01);
}

#[test]
fn degraded_report_emits_top_three_bottlenecks_in_severity_order() {
    let mut r = empty_report();
    r.tasks_done = 5;
    r.counts.done = 5;
    r.counts.blocked = 4;
    r.top_blocked = vec![("quota".into(), 3), ("gate_failed".into(), 1)];
    r.stages = vec![stage("running", 120), stage("blocked", 480)];
    r.autonomy = AutonomyMetrics {
        done_tasks: 5,
        done_without_human: 3,
        autonomy_rate_pct: Some(60.0),
        gate_tasks_evaluated: 5,
        gate_first_pass_clear: 2,
        gate_first_pass_rate_pct: Some(40.0),
        cross_audit_total: 5,
        cross_audit_failed: 1,
        cross_audit_fail_rate_pct: Some(20.0),
        implementer_escalations: 4,
        ..AutonomyMetrics::default()
    };

    let res = compute_factory_score(&r);
    assert_eq!(res.status, "ok");
    assert!(res.score.unwrap() < 60);
    assert_eq!(res.grade.as_deref(), Some("F"));
    assert_eq!(res.bottlenecks.len(), 3);
    let ranks: Vec<u8> = res
        .bottlenecks
        .iter()
        .map(|b| match b.severity.as_str() {
            "high" => 3,
            "medium" => 2,
            _ => 1,
        })
        .collect();
    assert!(ranks.windows(2).all(|w| w[0] >= w[1]));
    assert!(res.bottlenecks.iter().any(|b| b.key == "quota_blocks"));
    assert!(
        res.bottlenecks
            .iter()
            .any(|b| b.key == "low_first_pass_gate")
    );
}

#[test]
fn escalations_lightly_penalize_autonomy_discipline() {
    let mut base = empty_report();
    base.tasks_done = 4;
    base.counts.done = 4;
    base.autonomy = AutonomyMetrics {
        done_tasks: 4,
        done_without_human: 4,
        autonomy_rate_pct: Some(100.0),
        gate_tasks_evaluated: 4,
        gate_first_pass_clear: 4,
        gate_first_pass_rate_pct: Some(100.0),
        ..AutonomyMetrics::default()
    };
    let mut esc = base.clone();
    esc.autonomy.implementer_escalations = 4;

    let p_clean = compute_factory_score(&base)
        .pillars
        .into_iter()
        .find(|p| p.key == "autonomy_discipline")
        .unwrap();
    let p_esc = compute_factory_score(&esc)
        .pillars
        .into_iter()
        .find(|p| p.key == "autonomy_discipline")
        .unwrap();
    assert!(p_esc.score < p_clean.score);
    assert!(p_esc.score > p_clean.max_score * 0.7);
}
