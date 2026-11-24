//! Dual-Review handover consensus checks, clean `what_done` extraction, and HTML
//! table renderer for per-reviewer verdicts.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::gate::PostGateAuditRecord;
use aaf_domain::pipeline::{ReviewerVerdictRecord, distinct_real_canonical_providers};
use aaf_domain::task::Task;
use serde_json::Value;

use crate::cross_audit::scoring::{MechanicalScoreReport, evaluate_score_gate};
use crate::handover_html::escape_html;

const INTERNAL_NOTE_PREFIXES: &[&str] = &[
    "unblocked:",
    "rework:",
    "steer:",
    "post-gate cross-audit rejected",
    "cross-audit rejected",
    "audit_rework:",
    "plan_approved:",
    "unblock[",
    "gate failed:",
    "merge conflict:",
];

/// Verifies Dual-Review consensus requirements for handover readiness (`XONG`).
pub fn check_dual_review_missing(
    audit: Option<&PostGateAuditRecord>,
    dual_review: bool,
    require_different_provider: bool,
    score_gate: bool,
    min_avg: u8,
    min_crit: u8,
) -> Vec<String> {
    if !dual_review {
        return Vec::new();
    }
    let Some(record) = audit else {
        return vec![
            "missing verified dual-review consensus audit record (dual_review = true)".into(),
        ];
    };
    let mut missing = Vec::new();
    if !record.verdict.trim().eq_ignore_ascii_case("pass") {
        missing.push(format!(
            "dual-review verdict is {:?} (expected PASS)",
            record.verdict
        ));
    }
    if !record.dual_consensus {
        missing.push("missing dual-review consensus (dual_consensus = false)".into());
    }
    if record.reviewers.len() < 2 {
        missing.push(format!(
            "dual-review requires at least 2 reviewers, found {}",
            record.reviewers.len()
        ));
    }
    if require_different_provider {
        let providers =
            distinct_real_canonical_providers(record.reviewers.iter().map(|r| r.executor.as_str()));
        if providers < 2 {
            missing.push(format!(
                "dual-review requires >= 2 distinct real canonical providers, found {providers}"
            ));
        }
    }
    let mech = MechanicalScoreReport {
        score: record.mechanical_score,
        reasons: Vec::new(),
        same_account_warning: record.same_account && !require_different_provider,
    };
    let mut seen_ids = BTreeSet::new();
    for r in &record.reviewers {
        let id = r.id.trim();
        if id.is_empty() {
            missing.push("dual-review reviewer has empty id".into());
        } else if !seen_ids.insert(id) {
            missing.push(format!("dual-review duplicate reviewer id {id:?}"));
        }
        if !r.verdict.trim().eq_ignore_ascii_case("pass") {
            missing.push(format!(
                "dual-review reviewer {} verdict is {} (expected PASS)",
                r.id, r.verdict
            ));
        }
        if r.p0 > 0 || r.p1 > 0 {
            missing.push(format!(
                "dual-review reviewer {} reported P0={} P1={} (expected 0)",
                r.id, r.p0, r.p1
            ));
        }
        if score_gate {
            let eval = evaluate_score_gate(&mech, &r.scores, min_avg, min_crit);
            if !eval.passed {
                missing.push(format!(
                    "dual-review reviewer {} failed score gate: {}",
                    r.id,
                    eval.reasons.join("; ")
                ));
            }
        }
    }
    missing
}

fn is_internal_note(note: &str) -> bool {
    let lower = note.trim().to_ascii_lowercase();
    INTERNAL_NOTE_PREFIXES.iter().any(|p| lower.starts_with(p))
}

fn one_line(s: &str, max_chars: usize) -> String {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = collapsed.chars().take(max_chars).collect();
    if collapsed.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

fn latest_event(events: &[FactoryEvent], kind: EventKind) -> Option<&FactoryEvent> {
    events
        .iter()
        .filter(|e| e.kind == kind)
        .max_by_key(|e| (e.created_at, e.id.unwrap_or(0)))
}

fn extract_plan_summary(event: &FactoryEvent) -> Option<String> {
    let value: Value = serde_json::from_str(&event.payload_json).ok()?;
    let raw = value
        .get("plan_contract")
        .and_then(|c| c.get("summary"))
        .and_then(Value::as_str)
        .or_else(|| value.get("summary").and_then(Value::as_str))
        .or_else(|| value.get("plan").and_then(Value::as_str))?;
    let clean = one_line(raw, 220);
    (!clean.is_empty()).then_some(clean)
}

/// Builds human-facing `what_done` segments, preferring `PlanContract.summary`
/// from the latest [`EventKind::PlanConverged`] and filtering internal notes.
pub fn build_clean_what_done(task: &Task, events: &[FactoryEvent]) -> Vec<String> {
    let mut parts = vec![format!(
        "{} (task {}, state: {})",
        one_line(&task.title, 160),
        task.id,
        task.state.as_str()
    )];
    let desc = one_line(&task.spec.description, 220);
    if !desc.is_empty() {
        parts.push(format!("Spec: {desc}"));
    }
    if !task.spec.acceptance.is_empty() {
        parts.push(format!("AC: {} item(s)", task.spec.acceptance.len()));
    }
    if let Some(summary) =
        latest_event(events, EventKind::PlanConverged).and_then(extract_plan_summary)
    {
        parts.push(format!("Plan: {summary}"));
    } else if let Some(note) = task
        .context_notes
        .iter()
        .rev()
        .filter(|n| !is_internal_note(n))
        .map(|n| one_line(n, 180))
        .find(|n| !n.is_empty())
    {
        parts.push(format!("Notes: {note}"));
    } else if let Some(plan) =
        latest_event(events, EventKind::PlanGenerated).and_then(extract_plan_summary)
    {
        parts.push(format!("Plan: {plan}"));
    }
    parts
}

/// Renders an offline-safe HTML section containing the Dual-Review reviewer table
/// (plus an `adjudicated` badge when `adjudicated == true`).
pub fn render_dual_reviewers_html(
    reviewers: &[ReviewerVerdictRecord],
    adjudicated: bool,
) -> String {
    if reviewers.is_empty() {
        return String::new();
    }
    let mut html = String::with_capacity(512);
    let badge = if adjudicated {
        " <span class=\"badge ok\">adjudicated</span>"
    } else {
        ""
    };
    let _ = writeln!(
        html,
        "  <section class=\"card\"><h2>Dual-Review Reviewers{badge}</h2>\
         <table><thead><tr><th>ID</th><th>Executor / Model</th><th>Verdict</th>\
         <th>P0 / P1 / P2</th><th>LLM Avg</th></tr></thead><tbody>"
    );
    for r in reviewers {
        let model = r.model.trim();
        let exec_model = if model.is_empty() {
            r.executor.clone()
        } else {
            format!("{}/{model}", r.executor)
        };
        let llm = r
            .llm_average
            .map_or_else(|| "n/a".to_string(), |v| format!("{v}/100"));
        let _ = writeln!(
            html,
            "      <tr><td><code>{}</code></td><td><code>{}</code></td><td>{}</td><td>{}/{}/{}</td><td>{}</td></tr>",
            escape_html(&r.id),
            escape_html(&exec_model),
            escape_html(&r.verdict),
            r.p0,
            r.p1,
            r.p2,
            escape_html(&llm)
        );
    }
    let _ = writeln!(html, "    </tbody></table>\n  </section>");
    html
}
