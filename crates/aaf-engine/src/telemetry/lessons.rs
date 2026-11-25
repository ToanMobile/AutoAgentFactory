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
    Ok(compute_lessons(&events))
}

/// Pure, deterministic aggregator over factory events in a report window.
pub fn compute_lessons(events: &[FactoryEvent]) -> Vec<LessonSuggestion> {
    let mut counts: BTreeMap<(String, String), u64> = BTreeMap::new();
    for event in events {
        let Ok(val) = serde_json::from_str::<Value>(&event.payload_json) else {
            continue;
        };
        match event.kind {
            EventKind::GateResult => record_gate_failures(&val, &mut counts),
            EventKind::CrossAudit => record_audit_failures(&val, &mut counts),
            EventKind::TaskStateChanged => record_blocked_transitions(&val, &mut counts),
            _ => {}
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
        .take(64)
        .collect::<String>()
        .to_ascii_lowercase()
}

fn record_gate_failures(val: &Value, counts: &mut BTreeMap<(String, String), u64>) {
    let clear = val
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|s| s.eq_ignore_ascii_case("clear"))
        || val.get("passed").and_then(Value::as_bool) == Some(true);
    if clear {
        return;
    }
    if let Some(failed) = val.get("failed_checks").and_then(Value::as_array) {
        let mut recorded = false;
        for item in failed {
            if let Some(name) = item.as_str() {
                bump(counts, "gate", name);
                recorded = true;
            }
        }
        if recorded {
            return;
        }
    }
    if let Some(reason) = val
        .get("reason")
        .or_else(|| val.get("check"))
        .or_else(|| val.get("status"))
        .and_then(Value::as_str)
    {
        bump(counts, "gate", reason);
    }
}

fn record_audit_failures(val: &Value, counts: &mut BTreeMap<(String, String), u64>) {
    let verdict = val.get("verdict").and_then(Value::as_str).unwrap_or("");
    if verdict.eq_ignore_ascii_case("pass") || verdict.eq_ignore_ascii_case("clean") {
        return;
    }
    if let Some(findings) = val.get("findings").and_then(Value::as_array) {
        let mut recorded = false;
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
        if recorded {
            return;
        }
    }
    if !verdict.is_empty() {
        bump(counts, "cross_audit", verdict);
    }
}

fn record_blocked_transitions(val: &Value, counts: &mut BTreeMap<(String, String), u64>) {
    let to = val.get("to").and_then(Value::as_str).unwrap_or("");
    if !to.eq_ignore_ascii_case("blocked") {
        return;
    }
    let kind = val
        .get("reason_kind")
        .or_else(|| val.get("blocked_reason"))
        .or_else(|| val.get("kind"))
        .and_then(Value::as_str)
        .or_else(|| {
            val.get("why")
                .and_then(Value::as_str)
                .and_then(|why| why.split(':').next())
        });
    if let Some(reason) = kind {
        bump(counts, "blocked", reason);
    }
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
        ("blocked", "merge_conflict" | "rebase_conflict") => {
            "Keep task diffs tightly scoped to avoid overlapping edits and rebase conflicts."
                .to_string()
        }
        ("blocked", "missing_proof" | "proof_missing") => {
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
    fn groups_recurring_failures_and_drops_one_off_noise() {
        let events = vec![
            ev(
                EventKind::GateResult,
                r#"{"status":"blocked","failed_checks":["lint"]}"#,
                10,
            ),
            ev(
                EventKind::GateResult,
                r#"{"status":"blocked","failed_checks":["lint"]}"#,
                20,
            ),
            ev(
                EventKind::GateResult,
                r#"{"status":"blocked","failed_checks":["lint"]}"#,
                30,
            ),
            ev(
                EventKind::GateResult,
                r#"{"status":"blocked","failed_checks":["one_off_check"]}"#,
                40,
            ),
            ev(
                EventKind::CrossAudit,
                r#"{"verdict":"fail","findings":[{"id":"ASSERT"}]}"#,
                50,
            ),
            ev(
                EventKind::CrossAudit,
                r#"{"verdict":"fail","findings":[{"id":"ASSERT"}]}"#,
                60,
            ),
            ev(
                EventKind::TaskStateChanged,
                r#"{"to":"blocked","reason_kind":"head_lock"}"#,
                70,
            ),
            ev(
                EventKind::TaskStateChanged,
                r#"{"to":"blocked","reason_kind":"head_lock"}"#,
                80,
            ),
            // Passing events must never become failure lessons.
            ev(EventKind::GateResult, r#"{"status":"clear"}"#, 90),
            ev(EventKind::CrossAudit, r#"{"verdict":"pass"}"#, 95),
        ];

        let lessons = compute_lessons(&events);
        assert_eq!(lessons.len(), 3);
        assert_eq!(lessons[0].category, "gate");
        assert_eq!(lessons[0].key, "lint");
        assert_eq!(lessons[0].occurrences, 3);
        assert!(
            lessons
                .iter()
                .any(|l| l.key == "assert" && l.occurrences == 2)
        );
        assert!(
            lessons
                .iter()
                .any(|l| l.key == "head_lock" && l.occurrences == 2)
        );
        assert!(!lessons.iter().any(|l| l.key == "one_off_check"));
    }
}
