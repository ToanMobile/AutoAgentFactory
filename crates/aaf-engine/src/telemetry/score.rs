//! Deterministic post-run factory auto-scoring and bottleneck insights.
//!
//! Pure, read-only aggregation over [`FactoryReport`]: zero LLM calls and zero I/O.

use serde::{Deserialize, Serialize};

use super::report::{FactoryReport, QUEUED_STAGE, StageStat};

const MAX_BOTTLENECKS: usize = 3;
const PILLARS: [(&str, &str, f64); 5] = [
    ("first_pass_gate", "First-Pass Gate", 30.0),
    ("autonomy_discipline", "Autonomy Discipline", 25.0),
    ("audit_integrity", "Cross-Audit Integrity", 20.0),
    ("block_health", "Block Health", 15.0),
    ("flow_efficiency", "Flow Efficiency", 10.0),
];

type RankedHint = (u8, f64, BottleneckHint);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScorePillar {
    pub key: String,
    pub label: String,
    pub score: f64,
    pub max_score: f64,
    pub weight_pct: f64,
    pub active: bool,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BottleneckHint {
    pub severity: String,
    pub key: String,
    pub title: String,
    pub detail: String,
    pub recommendation: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FactoryScore {
    pub status: String,
    pub score: Option<u8>,
    pub grade: Option<String>,
    pub evaluated_tasks: u64,
    pub pillars: Vec<ScorePillar>,
    pub bottlenecks: Vec<BottleneckHint>,
}

/// Computes a deterministic 0..=100 score, letter grade, pillar breakdown, and
/// top actionable bottlenecks from a [`FactoryReport`].
pub fn compute_factory_score(report: &FactoryReport) -> FactoryScore {
    let a = &report.autonomy;
    if a.done_tasks == 0 && a.gate_tasks_evaluated == 0 && report.tasks_done == 0 {
        return insufficient_data_score();
    }
    let evaluated_tasks = report
        .tasks_done
        .max(a.done_tasks)
        .max(a.gate_tasks_evaluated);
    let evaluated = evaluate_pillars(report);
    let active_base: f64 = evaluated
        .iter()
        .enumerate()
        .filter(|(_, (active, _, _))| *active)
        .map(|(i, _)| PILLARS[i].2)
        .sum();
    if active_base <= 0.0 {
        return insufficient_data_score();
    }
    let mut raw_total = 0.0_f64;
    let pillars = evaluated
        .into_iter()
        .enumerate()
        .map(|(i, (active, rate, detail))| {
            let (key, label, base_w) = PILLARS[i];
            if !active {
                return inactive_pillar(key, label, detail);
            }
            let norm_w = (base_w / active_base) * 100.0;
            raw_total += (base_w / active_base) * rate;
            let weight_pct = round2(norm_w);
            let score = round2(norm_w * (rate / 100.0)).min(weight_pct);
            ScorePillar {
                key: key.into(),
                label: label.into(),
                score,
                max_score: weight_pct,
                weight_pct,
                active: true,
                detail,
            }
        })
        .collect();
    let final_score = raw_total.round().clamp(0.0, 100.0) as u8;
    FactoryScore {
        status: "ok".into(),
        score: Some(final_score),
        grade: Some(grade_for(final_score).into()),
        evaluated_tasks,
        pillars,
        bottlenecks: detect_bottlenecks(report),
    }
}

fn insufficient_data_score() -> FactoryScore {
    let pillars = PILLARS
        .into_iter()
        .map(|(k, l, _)| inactive_pillar(k, l, "no evaluated tasks in window".into()))
        .collect();
    FactoryScore {
        status: "insufficient_data".into(),
        score: None,
        grade: None,
        evaluated_tasks: 0,
        pillars,
        bottlenecks: Vec::new(),
    }
}

fn inactive_pillar(key: &str, label: &str, detail: String) -> ScorePillar {
    ScorePillar {
        key: key.into(),
        label: label.into(),
        score: 0.0,
        max_score: 0.0,
        weight_pct: 0.0,
        active: false,
        detail,
    }
}

fn evaluate_pillars(report: &FactoryReport) -> [(bool, f64, String); 5] {
    let a = &report.autonomy;
    let gate = if a.gate_tasks_evaluated == 0 {
        (false, 0.0, "no gate evaluations in window".into())
    } else {
        let (clr, tot) = (a.gate_first_pass_clear, a.gate_tasks_evaluated);
        let r = resolve_rate(a.gate_first_pass_rate_pct, clr, tot);
        (
            true,
            r,
            format!("{clr}/{tot} clear on first pass ({r:.1}%)"),
        )
    };
    let autonomy = if a.done_tasks == 0 {
        (false, 0.0, "no completed tasks with event history".into())
    } else {
        let (ok, tot, esc) = (
            a.done_without_human,
            a.done_tasks,
            a.implementer_escalations,
        );
        let base = resolve_rate(a.autonomy_rate_pct, ok, tot);
        let pen = ((esc as f64 / tot as f64) * 15.0).min(25.0);
        let d = format!("{ok}/{tot} autonomous ({base:.1}%), {esc} escalation(s)");
        (true, (base - pen).clamp(0.0, 100.0), d)
    };
    let audit = if a.cross_audit_total == 0 {
        (false, 0.0, "no cross-audits recorded".into())
    } else {
        let (fn_, tot) = (a.cross_audit_failed, a.cross_audit_total);
        let fail = resolve_rate(a.cross_audit_fail_rate_pct, fn_, tot);
        let d = format!("{fn_}/{tot} audits failed ({fail:.1}% fail rate)");
        (true, (100.0 - fail).clamp(0.0, 100.0), d)
    };
    let (done, blocked, weighted) = block_summary(report);
    let block = if done == 0 && blocked == 0 {
        (false, 0.0, "no completed or blocked tasks".into())
    } else {
        let denom = done as f64 + weighted;
        let r = if denom > 0.0 {
            ((done as f64 / denom) * 100.0).clamp(0.0, 100.0)
        } else {
            100.0
        };
        let d = match (blocked, report.top_blocked.first()) {
            (0, _) => format!("{done} completed, 0 blocked"),
            (_, Some((k, n))) => format!("{done} completed vs {blocked} blocked ({k}={n})"),
            _ => format!("{done} completed vs {blocked} blocked"),
        };
        (true, r, d)
    };
    let (act, wait, _) = stage_timing_summary(&report.stages);
    let total_secs = act.saturating_add(wait);
    let flow = if total_secs == 0 {
        (false, 0.0, "no stage timing data".into())
    } else {
        let eff = ((act as f64 / total_secs as f64) * 100.0).clamp(0.0, 100.0);
        let d = format!("{act}s active vs {wait}s wait/blocked ({eff:.1}%)");
        (true, eff, d)
    };
    [gate, autonomy, audit, block, flow]
}

fn detect_bottlenecks(report: &FactoryReport) -> Vec<BottleneckHint> {
    let mut hints = Vec::new();
    let a = &report.autonomy;
    if a.gate_tasks_evaluated > 0 {
        let (clr, tot) = (a.gate_first_pass_clear, a.gate_tasks_evaluated);
        let r = resolve_rate(a.gate_first_pass_rate_pct, clr, tot);
        if let Some(sev) = sev_tier(r, [60.0, 80.0, 90.0], true) {
            let d = format!("{clr}/{tot} cleared gate on first attempt ({r:.1}%)");
            push_static(&mut hints, sev, 100.0 - r, "low_first_pass_gate", d);
        }
    }
    let (done, blocked, _) = block_summary(report);
    let denom = done.saturating_add(blocked).max(1) as f64;
    let quota: u64 = report
        .top_blocked
        .iter()
        .filter(|(k, _)| k == "quota")
        .map(|(_, n)| *n)
        .fold(0, u64::saturating_add);
    if quota > 0 {
        let ratio = quota as f64 / denom;
        let sev = if quota >= 2 || ratio >= 0.25 {
            "high"
        } else {
            "medium"
        };
        let imp = (ratio * 100.0).max(quota as f64 * 20.0);
        let d = format!("{quota} task(s) blocked by quota/rate limits");
        push_static(&mut hints, sev, imp, "quota_blocks", d);
    }
    let rem = blocked.saturating_sub(quota);
    let non_quota = report
        .top_blocked
        .iter()
        .find(|(k, n)| k != "quota" && *n > 0)
        .map(|(k, n)| (k.as_str(), *n))
        .or_else(|| (rem > 0).then_some(("unknown", rem)));
    if let Some((kind, cnt)) = non_quota {
        let ratio = cnt as f64 / denom;
        let sev_kind = matches!(kind, "gate_failed" | "stuck" | "infra" | "budget_exceeded");
        let sev = if sev_kind && (cnt >= 2 || ratio >= 0.25) {
            "high"
        } else if cnt >= 2 || ratio >= 0.20 {
            "medium"
        } else {
            "low"
        };
        let imp = (ratio * 100.0).max(cnt as f64 * 15.0);
        let t = format!("Tasks blocked by `{kind}`");
        let d = format!("{cnt} of {blocked} blocked task(s) stalled on `{kind}`");
        push_hint(
            &mut hints,
            sev,
            imp,
            "dominant_block_reason",
            t,
            d,
            rec_for_block(kind),
        );
    }
    if a.cross_audit_total > 0 && a.cross_audit_failed > 0 {
        let (fn_, tot) = (a.cross_audit_failed, a.cross_audit_total);
        let fail = resolve_rate(a.cross_audit_fail_rate_pct, fn_, tot);
        if let Some(sev) = sev_tier(fail, [35.0, 15.0, 10.0], false) {
            let d = format!("{fn_}/{tot} cross-audits failed ({fail:.1}%)");
            push_static(&mut hints, sev, fail, "high_cross_audit_fail", d);
        }
    }
    if a.done_tasks > 0 {
        let (ok, tot, esc_n) = (
            a.done_without_human,
            a.done_tasks,
            a.implementer_escalations,
        );
        let auto = resolve_rate(a.autonomy_rate_pct, ok, tot);
        if auto < 85.0 {
            let sev = if auto < 65.0 { "high" } else { "medium" };
            let d = format!("{ok}/{tot} tasks finished autonomously ({auto:.1}%)");
            push_static(&mut hints, sev, 100.0 - auto, "low_autonomy", d);
        }
        let esc = esc_n as f64 / tot as f64;
        if let Some(sev) = sev_tier(esc, [0.75, 0.35, 0.20], false)
            && (sev != "low" || esc_n >= 2)
        {
            let d = format!("{esc_n} escalation(s) across {tot} tasks ({esc:.2}/task)");
            push_static(
                &mut hints,
                sev,
                (esc * 50.0).min(100.0),
                "excessive_escalations",
                d,
            );
        }
    }
    let (act, wait, top_wait) = stage_timing_summary(&report.stages);
    let total = act.saturating_add(wait);
    if total > 0 && wait > 0 {
        let pct = (wait as f64 / total as f64) * 100.0;
        if let Some(sev) = sev_tier(pct, [65.0, 40.0, 25.0], false)
            && let Some((st, secs)) = top_wait
        {
            let t = format!("Dominant wait time in `{st}` stage");
            let d = format!("{pct:.1}% of stage time waiting/blocked (`{st}`: {secs}s)");
            push_hint(
                &mut hints,
                sev,
                pct,
                "dominant_stage_wait",
                t,
                d,
                rec_for_stage(st),
            );
        }
    }
    hints.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.total_cmp(&a.1))
            .then_with(|| a.2.key.cmp(&b.2.key))
    });
    hints.truncate(MAX_BOTTLENECKS);
    hints.into_iter().map(|(_, _, h)| h).collect()
}

fn push_static(out: &mut Vec<RankedHint>, sev: &str, imp: f64, key: &str, detail: String) {
    let (title, rec) = match key {
        "low_first_pass_gate" => (
            "Low first-pass gate clear rate",
            "Tighten pre-gate checks, specs, or lint hooks.",
        ),
        "quota_blocks" => (
            "Quota or account cooldown blocks",
            "Add backup accounts, spread pools, or cut concurrency.",
        ),
        "high_cross_audit_fail" => (
            "High cross-audit failure rate",
            "Codify recurring audit findings into rules or gates.",
        ),
        "low_autonomy" => (
            "Frequent human intervention required",
            "Audit undo/steer/unblock events to automate recovery.",
        ),
        _ => (
            "Excessive implementer escalations",
            "Route harder tasks to higher-tier executors or refine specs.",
        ),
    };
    push_hint(out, sev, imp, key, title.into(), detail, rec);
}

fn push_hint(
    out: &mut Vec<RankedHint>,
    sev: &str,
    impact: f64,
    key: &str,
    title: String,
    detail: String,
    rec: &str,
) {
    let rank = match sev {
        "high" => 3,
        "medium" => 2,
        _ => 1,
    };
    let score = if impact.is_finite() { impact } else { 0.0 };
    let hint = BottleneckHint {
        severity: sev.into(),
        key: key.into(),
        title,
        detail,
        recommendation: rec.into(),
    };
    out.push((rank, score, hint));
}

fn block_summary(report: &FactoryReport) -> (u64, u64, f64) {
    let done = report
        .tasks_done
        .max(report.autonomy.done_tasks)
        .max(u64::from(report.counts.done));
    let top_sum = report
        .top_blocked
        .iter()
        .map(|(_, n)| *n)
        .fold(0u64, u64::saturating_add);
    let blocked = u64::from(report.counts.blocked).max(top_sum);
    let weighted: f64 = report
        .top_blocked
        .iter()
        .map(|(k, n)| match k.as_str() {
            "quota" | "gate_failed" | "stuck" | "infra" | "budget_exceeded" => *n as f64 * 1.2,
            "dependency" => *n as f64 * 0.7,
            _ => *n as f64,
        })
        .sum();
    (
        done,
        blocked,
        weighted + blocked.saturating_sub(top_sum) as f64,
    )
}

fn stage_timing_summary(stages: &[StageStat]) -> (u64, u64, Option<(&str, u64)>) {
    let (mut active, mut wait, mut top) = (0u64, 0u64, None::<(&str, u64)>);
    for st in stages {
        let secs = st.durations.total_secs;
        match st.stage.as_str() {
            "running" | "gating" => active = active.saturating_add(secs),
            QUEUED_STAGE | "backlog" | "ready" | "review" | "blocked" => {
                wait = wait.saturating_add(secs);
                if secs > 0
                    && top
                        .is_none_or(|(bs, bn)| secs > bn || (secs == bn && st.stage.as_str() < bs))
                {
                    top = Some((st.stage.as_str(), secs));
                }
            }
            _ => {}
        }
    }
    (active, wait, top)
}

fn rec_for_block(kind: &str) -> &'static str {
    match kind {
        "gate_failed" => "Inspect recurring gate failures and tighten fixtures or specs.",
        "stuck" => "Split oversized tasks into smaller scoped subtasks.",
        "infra" => "Run `aaf doctor` to verify worktree, git, and toolchain health.",
        "merge_conflict" => "Assign disjoint scope_paths or reduce parallel shared-file edits.",
        "dependency" => "Prioritize upstream blocker tasks to unblock DAG dependents.",
        _ => "Inspect blocked tasks with `aaf status` and unblock or re-scope.",
    }
}

fn rec_for_stage(stage: &str) -> &'static str {
    match stage {
        "blocked" => "Resolve top block reasons and enable safe auto-retry.",
        "review" => "Streamline PR review or auto-merge gates to drain review queue.",
        QUEUED_STAGE | "ready" | "backlog" => "Increase worker concurrency or prune stale backlog.",
        _ => "Inspect stage transitions in `aaf report` to reduce dwell time.",
    }
}

fn sev_tier(v: f64, [h, m, l]: [f64; 3], below: bool) -> Option<&'static str> {
    let hit = |bound| if below { v < bound } else { v >= bound };
    hit(h)
        .then_some("high")
        .or_else(|| hit(m).then_some("medium"))
        .or_else(|| hit(l).then_some("low"))
}

fn resolve_rate(explicit: Option<f64>, num: u64, den: u64) -> f64 {
    match explicit {
        Some(v) if v.is_finite() => v.clamp(0.0, 100.0),
        _ if den > 0 => ((num as f64 * 100.0) / den as f64).clamp(0.0, 100.0),
        _ => 0.0,
    }
}

fn grade_for(score: u8) -> &'static str {
    match score {
        90..=100 => "A",
        80..=89 => "B",
        70..=79 => "C",
        60..=69 => "D",
        _ => "F",
    }
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
