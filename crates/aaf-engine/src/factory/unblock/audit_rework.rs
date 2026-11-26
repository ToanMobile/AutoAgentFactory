//! Automatic audit-rework routing, score-plateau stop, and finding formatting.

use aaf_domain::budget::BudgetCheck;
use aaf_domain::config::MAX_AUDIT_REWORKS_CAP;
use aaf_domain::event::EventKind;
use aaf_domain::ports::PortResult;
use aaf_domain::task::{Task, TaskState};
use serde_json::json;

use super::super::Deps;
use super::super::rework_filter::is_valid_rework_finding;
use super::super::worker::{Next, ensure_budget};
use super::{is_human_unblock_event, one_line};
use crate::cross_audit::Finding;

const AUDIT_FINDINGS_MAX_CHARS: usize = 3600;
const PER_FINDING_MAX_CHARS: usize = 450;

/// Neutralises any closing `</untrusted-data` tag (ASCII case-insensitive) so
/// untrusted audit findings cannot escape their wrapper block.
pub(crate) fn sanitize_untrusted_close_tag(text: &str) -> String {
    const NEEDLE: &[u8] = b"</untrusted-data";
    let (bytes, mut out, mut copied, mut at) =
        (text.as_bytes(), String::with_capacity(text.len()), 0, 0);
    while at + NEEDLE.len() <= bytes.len() {
        if bytes[at..at + NEEDLE.len()].eq_ignore_ascii_case(NEEDLE) {
            out.push_str(&text[copied..at]);
            out.push_str("&lt;/untrusted-data");
            at += NEEDLE.len();
            copied = at;
        } else {
            at += 1;
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// Counts explicit failing `CrossAudit` events for `task_id` since the latest
/// `ReworkRequested` or human unblock (`Blocked -> Ready`) using a filtered
/// reverse scan so counts are never dropped past 50 events.
pub(crate) fn audit_fails_since_reset(deps: &Deps, task_id: &str) -> PortResult<u32> {
    let mut count: u32 = 0;
    let kinds = [
        EventKind::CrossAudit,
        EventKind::TaskStateChanged,
        EventKind::ReworkRequested,
    ];
    for event in deps
        .store
        .events_for_task_by_kinds(task_id, &kinds, u32::MAX)?
    {
        if event.kind == EventKind::ReworkRequested || is_human_unblock_event(&event) {
            break;
        }
        if event.kind == EventKind::CrossAudit
            && serde_json::from_str::<serde_json::Value>(&event.payload_json)
                .ok()
                .is_some_and(|p| {
                    p.get("reused_from").is_none_or(serde_json::Value::is_null)
                        && p["verdict"]
                            .as_str()
                            .is_some_and(|v| v == "fail" || v.starts_with("fail:"))
                })
        {
            count = count.saturating_add(1);
        }
    }
    Ok(count)
}

/// True when the latest three scored post-gate audit failures since reset show
/// no score improvement across two consecutive reworks (`s_{n-2} >= s_{n-1} >= s_n`).
pub(crate) fn has_audit_score_plateau(deps: &Deps, task_id: &str) -> PortResult<bool> {
    let mut scores_rev: Vec<u64> = Vec::new();
    let kinds = [
        EventKind::CrossAudit,
        EventKind::TaskStateChanged,
        EventKind::ReworkRequested,
    ];
    for event in deps
        .store
        .events_for_task_by_kinds(task_id, &kinds, u32::MAX)?
    {
        if event.kind == EventKind::ReworkRequested || is_human_unblock_event(&event) {
            break;
        }
        if event.kind != EventKind::CrossAudit {
            continue;
        }
        let Ok(p) = serde_json::from_str::<serde_json::Value>(&event.payload_json) else {
            continue;
        };
        if p["phase"].as_str() != Some("post_gate")
            || p.get("reused_from").is_some_and(|v| !v.is_null())
        {
            continue;
        }
        let is_fail = p["verdict"]
            .as_str()
            .is_some_and(|v| v == "fail" || v.starts_with("fail:"));
        if is_fail && let Some(score) = p["effective_score"].as_u64() {
            scores_rev.push(score);
            if scores_rev.len() >= 3 {
                break;
            }
        }
    }
    // `scores_rev` is newest-first: [s_n, s_{n-1}, s_{n-2}].
    Ok(scores_rev.len() >= 3 && scores_rev[0] <= scores_rev[1] && scores_rev[1] <= scores_rev[2])
}

pub(crate) fn format_sanitized_audit_findings(findings: &[Finding]) -> Option<String> {
    let raw = findings
        .iter()
        .filter(|f| is_valid_rework_finding(f))
        .map(|f| {
            let entry = format!("{}: {} ({})", f.id, f.question, f.evidence.trim());
            one_line(&entry, PER_FINDING_MAX_CHARS)
        })
        .collect::<Vec<_>>()
        .join("; ");
    (!raw.is_empty())
        .then(|| sanitize_untrusted_close_tag(&one_line(&raw, AUDIT_FINDINGS_MAX_CHARS)))
}

/// Automatic audit-rework routing on explicit `AuditVerdict::Fail`.
pub(crate) fn handle_audit_failure(
    deps: &Deps,
    task: &Task,
    findings: &[Finding],
) -> PortResult<Option<Next>> {
    let cap = deps
        .settings
        .repo
        .pipeline
        .max_audit_reworks
        .min(MAX_AUDIT_REWORKS_CAP);
    if cap == 0
        || audit_fails_since_reset(deps, &task.id)?.saturating_sub(1) >= cap
        || has_audit_score_plateau(deps, &task.id)?
    {
        return Ok(None);
    }
    let (valid, dropped): (Vec<_>, Vec<_>) =
        findings.iter().partition(|f| is_valid_rework_finding(f));
    if !dropped.is_empty() {
        let items: Vec<_> = dropped
            .iter()
            .map(|f| {
                let reason = if f.evidence.trim().is_empty() {
                    "empty_evidence"
                } else {
                    "missing_file_line"
                };
                json!({ "id": f.id, "reason": reason })
            })
            .collect();
        deps.emit(
            EventKind::DetectorHit,
            &task.id,
            json!({ "source": "audit_rework_filter", "dropped_count": items.len(), "dropped": items }),
        )?;
    }
    let Some(safe) = (!valid.is_empty())
        .then(|| format_sanitized_audit_findings(findings))
        .flatten()
    else {
        return Ok(None);
    };
    let budget = ensure_budget(deps, &task.root_id)?;
    if matches!(
        budget.check(),
        BudgetCheck::Frozen | BudgetCheck::TokensExceeded | BudgetCheck::WallClockExceeded
    ) {
        return Ok(None);
    }
    let note =
        format!("audit_rework: <untrusted-data id=\"audit-findings\">{safe}</untrusted-data>");
    deps.store
        .append_context_note(&task.id, &note, deps.now())?;
    deps.move_to(&task.id, TaskState::Ready, None, "audit_rework")?;
    Ok(Some(Next::Ready))
}
