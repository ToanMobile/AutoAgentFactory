//! Deterministic, read-only lesson extraction from historical factory events
//! (`aaf report --lessons`).
//!
//! Never writes to `aaf.db`, never calls an LLM, and never auto-injects into
//! prompts: it surfaces recurring failure patterns (`occurrences >= 2`) as
//! copy-pasteable rules for an operator to review and commit to
//! `.autofactory/memory.md` (or `CLAUDE.md`).

use std::collections::{BTreeMap, HashSet};

use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{PortResult, TaskStore};
use aaf_domain::task::Task;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ReportWindow;
use crate::db::Db;

/// Minimum repeats within the report window before a pattern is surfaced.
pub const MIN_LESSON_OCCURRENCES: u64 = 2;
/// Maximum number of suggestions returned to keep output concise.
const MAX_LESSONS: usize = 10;

/// One recurring failure pattern distilled into an actionable rule suggestion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LessonSuggestion {
    pub category: String,
    pub key: String,
    pub occurrences: u64,
    pub suggested_rule: String,
}

/// Reads tasks and events for `repo_root` (or all repos) and returns recurring
/// failure patterns as candidate rules for `.autofactory/memory.md`.
pub fn collect_lessons(
    db: &Db,
    repo_root: Option<&str>,
    window: ReportWindow,
) -> PortResult<Vec<LessonSuggestion>> {
    let tasks: Vec<Task> = db
        .list_tasks(repo_root, None)?
        .into_iter()
        .filter(|t| {
            window
                .since
                .is_none_or(|s| t.created_at >= s || t.updated_at >= s)
        })
        .collect();
    let ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
    let events: Vec<FactoryEvent> = db
        .recent_events(repo_root, u32::MAX)?
        .into_iter()
        .filter(|e| {
            window.since.is_none_or(|s| e.created_at >= s)
                && e.task_id.as_deref().is_some_and(|id| ids.contains(id))
        })
        .collect();
    Ok(compute_lessons(&tasks, &events))
}

/// Pure, deterministic aggregator over tasks and factory events in a report window.
pub fn compute_lessons(tasks: &[Task], events: &[FactoryEvent]) -> Vec<LessonSuggestion> {
    let mut counts: BTreeMap<(String, String), u64> = BTreeMap::new();
    let mut blocked_from_events: HashSet<&str> = HashSet::new();
    for event in events {
        let Ok(val) = serde_json::from_str::<Value>(&event.payload_json) else {
            continue;
        };
        match event.kind {
            EventKind::GateResult => record_gate_failures(&val, &mut counts),
            EventKind::CrossAudit => record_audit_failures(&val, &mut counts),
            EventKind::TaskStateChanged => {
                if record_blocked_transitions(&val, &mut counts)
                    && let Some(tid) = event.task_id.as_deref()
                {
                    blocked_from_events.insert(tid);
                }
            }
            _ => {}
        }
    }
    // Include structured `Task::blocked_reason` for blocked tasks whose
    // `TaskStateChanged` event did not carry a specific reason key.
    for task in tasks {
        if !blocked_from_events.contains(task.id.as_str())
            && let Some(reason) = &task.blocked_reason
        {
            bump(&mut counts, "blocked", reason.kind());
        }
    }
    let mut out: Vec<LessonSuggestion> = counts
        .into_iter()
        .filter(|(_, n)| *n >= MIN_LESSON_OCCURRENCES)
        .map(|((category, key), occurrences)| {
            let suggested_rule = rule_for(&category, &key);
            LessonSuggestion {
                category,
                key,
                occurrences,
                suggested_rule,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.occurrences
            .cmp(&a.occurrences)
            .then_with(|| a.category.cmp(&b.category))
            .then_with(|| a.key.cmp(&b.key))
    });
    out.truncate(MAX_LESSONS);
    out
}

fn bump(counts: &mut BTreeMap<(String, String), u64>, category: &str, key: &str) {
    let norm = sanitize_key(key);
    if norm.is_empty() {
        return;
    }
    *counts.entry((category.to_string(), norm)).or_insert(0) += 1;
}

fn sanitize_key(raw: &str) -> String {
    raw.trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':' | '.'))
        .take(48)
        .collect::<String>()
        .to_ascii_lowercase()
}

fn classify_gate_reason(reason: &str) -> Option<&'static str> {
    let lower = reason.to_ascii_lowercase();
    if lower.contains("head lock") || lower.contains("head_lock") || lower.contains("locked") {
        Some("head_lock")
    } else if lower.contains("store") || lower.contains("compliance") {
        Some("store_compliance")
    } else if lower.contains("lint") || lower.contains("clippy") || lower.contains("fmt") {
        Some("lint")
    } else if lower.contains("test_scoped") || lower.contains("test") {
        Some("test")
    } else if lower.contains("build") || lower.contains("compile") {
        Some("build")
    } else if lower.contains("static") || lower.contains("critical") {
        Some("static_critical")
    } else if lower.contains("proof") || lower.contains("screenshot") {
        Some("missing_proof")
    } else {
        None
    }
}

/// Parses real `factory::finish::gate_payload` JSON (`status`, `reasons`,
/// `head_lock_status`, `store_status`, `static_critical`) as well as
/// structured `failed_checks`.
fn record_gate_failures(val: &Value, counts: &mut BTreeMap<(String, String), u64>) {
    let status = val.get("status").and_then(Value::as_str).unwrap_or("");
    let clear = status.eq_ignore_ascii_case("clear")
        || val.get("passed").and_then(Value::as_bool) == Some(true);
    if clear {
        return;
    }
    let mut recorded = false;
    if val
        .get("head_lock_status")
        .and_then(Value::as_str)
        .is_some_and(|s| s.eq_ignore_ascii_case("blocked"))
    {
        bump(counts, "gate", "head_lock");
        recorded = true;
    }
    if val
        .get("store_status")
        .and_then(Value::as_str)
        .is_some_and(|s| s.eq_ignore_ascii_case("blocked"))
    {
        bump(counts, "gate", "store_compliance");
        recorded = true;
    }
    if val
        .get("static_critical")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        > 0
    {
        bump(counts, "gate", "static_critical");
        recorded = true;
    }
    if let Some(reasons) = val.get("reasons").and_then(Value::as_array) {
        let mut seen = HashSet::new();
        for item in reasons.iter().filter_map(Value::as_str) {
            if let Some(kind) = classify_gate_reason(item)
                && seen.insert(kind)
            {
                bump(counts, "gate", kind);
                recorded = true;
            }
        }
    }
    if let Some(failed) = val.get("failed_checks").and_then(Value::as_array) {
        for name in failed.iter().filter_map(Value::as_str) {
            bump(counts, "gate", name);
            recorded = true;
        }
    }
    if !recorded && !status.is_empty() {
        bump(counts, "gate", status);
    }
}

/// Parses real `factory::audit_backend::verdict_text` (`"pass"`, `"fail: ID (evidence); ..."`,
/// `"inconclusive: ..."`, `"auth_wait"`) and structured `findings`.
fn record_audit_failures(val: &Value, counts: &mut BTreeMap<(String, String), u64>) {
    let verdict = val
        .get("verdict")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let lower = verdict.to_ascii_lowercase();
    if lower.is_empty()
        || matches!(lower.as_str(), "pass" | "clean" | "approved" | "auth_wait")
        || lower.starts_with("inconclusive")
    {
        return;
    }
    let mut recorded = false;
    if let Some(findings) = val.get("findings").and_then(Value::as_array) {
        for item in findings {
            if let Some(id) = item
                .get("id")
                .or_else(|| item.get("item_id"))
                .and_then(Value::as_str)
            {
                bump(counts, "cross_audit", id);
                recorded = true;
            }
        }
    }
    if let Some(rest) = verdict
        .strip_prefix("fail:")
        .or_else(|| verdict.strip_prefix("FAIL:"))
    {
        let mut seen = HashSet::new();
        for part in rest.split(';') {
            let id = part
                .trim()
                .split(['(', ':', ' '])
                .next()
                .unwrap_or("")
                .trim();
            if !id.is_empty() && seen.insert(id.to_ascii_lowercase()) {
                bump(counts, "cross_audit", id);
                recorded = true;
            }
        }
    }
    if !recorded {
        if lower.contains("mechanical score") || lower.contains("score") {
            bump(counts, "cross_audit", "low_score");
        } else {
            bump(counts, "cross_audit", "rejected");
        }
    }
}

fn classify_blocked_why(why: &str) -> Option<&'static str> {
    let lower = why.to_ascii_lowercase();
    if lower.contains("merge conflict") || lower.contains("rebase") {
        Some("merge_conflict")
    } else if lower.contains("head lock") || lower.contains("locked") {
        Some("head_lock")
    } else if lower.contains("proof") || lower.contains("screenshot") {
        Some("missing_proof")
    } else if lower.contains("budget") {
        Some("budget_exhausted")
    } else if lower.contains("cross-audit") || lower.contains("audit") {
        Some("cross_audit")
    } else if lower.contains("hook") {
        Some("hook_denied")
    } else {
        None
    }
}

fn record_blocked_transitions(val: &Value, counts: &mut BTreeMap<(String, String), u64>) -> bool {
    let to = val.get("to").and_then(Value::as_str).unwrap_or("");
    if !to.eq_ignore_ascii_case("blocked") {
        return false;
    }
    if let Some(kind) = val
        .get("reason_kind")
        .or_else(|| val.get("blocked_reason"))
        .and_then(Value::as_str)
    {
        bump(counts, "blocked", kind);
        return true;
    }
    if let Some(why) = val.get("why").and_then(Value::as_str)
        && let Some(kind) = classify_blocked_why(why)
    {
        bump(counts, "blocked", kind);
        return true;
    }
    false
}

fn rule_for(category: &str, key: &str) -> String {
    match (category, key) {
        ("gate", "lint" | "clippy" | "fmt") => {
            "Run the repository formatter and linter locally before committing each task."
                .to_string()
        }
        ("gate", "test" | "test_scoped" | "tests") => {
            "Run scoped tests before finishing; avoid committing partial or untested edits."
                .to_string()
        }
        ("gate", "build" | "compile") => {
            "Verify a clean build before committing; keep heavy builds wrapped with `aaf slot --`."
                .to_string()
        }
        ("gate", "head_lock") | ("blocked", "head_lock" | "locked_change") => {
            "Do not edit head-locked tests, CI, or config manifests unless the task spec requires it."
                .to_string()
        }
        ("gate", "static_critical") => {
            "Resolve critical static analysis findings (security/unwrap/suppression) before commit."
                .to_string()
        }
        ("gate", "store_compliance") => {
            "Check App Store / Play Store compliance rules before modifying auth, billing, or manifests."
                .to_string()
        }
        ("cross_audit", "assert") => {
            "Never weaken, delete, or skip existing test assertions to make a failing check pass."
                .to_string()
        }
        ("cross_audit", "test-branch") => {
            "Avoid test-only branches (`cfg!(test)` / env flags) inside production code paths."
                .to_string()
        }
        ("cross_audit", "stub") => {
            "Do not leave TODO/placeholder/stub implementations where real behaviour is required."
                .to_string()
        }
        ("cross_audit", "hardcoded") => {
            "Implement the general rule instead of hardcoding values to satisfy specific test cases."
                .to_string()
        }
        ("cross_audit", "secret") => {
            "Never commit hardcoded tokens, keys, passwords, or credentials; read them from env."
                .to_string()
        }
        ("cross_audit", "low_score") => {
            "Ensure diffs satisfy all acceptance criteria cleanly with tests and minimal churn."
                .to_string()
        }
        ("blocked", "merge_conflict" | "rebase_conflict") => {
            "Keep task diffs tightly scoped to avoid overlapping edits and rebase conflicts."
                .to_string()
        }
        ("blocked", "missing_proof" | "proof_missing") | ("gate", "missing_proof") => {
            "Capture required UI/device verification proof before concluding mobile UI tasks."
                .to_string()
        }
        _ => format!(
            "Address recurring `{category}:{key}` failures (add repo-specific guidance if needed)."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: EventKind, payload: &str, ts: i64) -> FactoryEvent {
        FactoryEvent {
            id: None,
            task_id: Some("t1".to_string()),
            run_id: None,
            kind,
            payload_json: payload.to_string(),
            created_at: ts,
        }
    }

    #[test]
    fn parses_real_gate_payload_verdict_text_and_blocked_events() {
        let events = vec![
            // Real `factory::finish::gate_payload` shape.
            ev(
                EventKind::GateResult,
                r#"{"status":"blocked","reasons":["lint failed (exit 1)","test_scoped failed"],"head_lock_status":"clear","store_status":"clear","static_critical":0}"#,
                10,
            ),
            ev(
                EventKind::GateResult,
                r#"{"status":"blocked","reasons":["lint failed (exit 1)"],"head_lock_status":"clear","store_status":"clear","static_critical":0}"#,
                20,
            ),
            // Real `factory::audit_backend::verdict_text` shape.
            ev(
                EventKind::CrossAudit,
                r#"{"verdict":"fail: ASSERT (removed check at src/a.rs:12); STUB (todo in src/b.rs)"}"#,
                30,
            ),
            ev(
                EventKind::CrossAudit,
                r#"{"verdict":"fail: ASSERT (weakened check at src/c.rs:8)"}"#,
                40,
            ),
            // Passing, inconclusive, and auth_wait events must never count as lessons.
            ev(EventKind::CrossAudit, r#"{"verdict":"pass"}"#, 45),
            ev(EventKind::CrossAudit, r#"{"verdict":"pass"}"#, 46),
            ev(EventKind::CrossAudit, r#"{"verdict":"approved"}"#, 47),
            ev(EventKind::CrossAudit, r#"{"verdict":"approved"}"#, 48),
            ev(
                EventKind::CrossAudit,
                r#"{"verdict":"inconclusive: timeout"}"#,
                49,
            ),
            ev(
                EventKind::CrossAudit,
                r#"{"verdict":"inconclusive: timeout"}"#,
                50,
            ),
            ev(EventKind::CrossAudit, r#"{"verdict":"auth_wait"}"#, 51),
            ev(EventKind::CrossAudit, r#"{"verdict":"auth_wait"}"#, 52),
            // Real `factory::Deps::move_to` shape.
            ev(
                EventKind::TaskStateChanged,
                r#"{"to":"blocked","why":"rebase failed with merge conflict on Cargo.toml"}"#,
                60,
            ),
            ev(
                EventKind::TaskStateChanged,
                r#"{"to":"blocked","why":"merge conflict in src/lib.rs"}"#,
                70,
            ),
        ];

        let lessons = compute_lessons(&[], &events);
        assert_eq!(lessons.len(), 3, "{lessons:?}");
        assert!(
            lessons
                .iter()
                .any(|l| l.category == "gate" && l.key == "lint" && l.occurrences == 2)
        );
        assert!(
            lessons
                .iter()
                .any(|l| l.category == "cross_audit" && l.key == "assert" && l.occurrences == 2)
        );
        assert!(lessons.iter().any(|l| {
            l.category == "blocked" && l.key == "merge_conflict" && l.occurrences == 2
        }));
        // One-off `test` and `stub`, plus `pass`/`approved`/`inconclusive`/`auth_wait`, are excluded.
        assert!(!lessons.iter().any(|l| l.key == "test" || l.key == "stub"));
        assert!(
            !lessons
                .iter()
                .any(|l| matches!(l.key.as_str(), "pass" | "approved" | "auth_wait"))
        );
    }
}
