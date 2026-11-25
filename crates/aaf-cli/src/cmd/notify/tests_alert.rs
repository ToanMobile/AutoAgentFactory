//! `alert_for` mapping of PR feedback events.

use aaf_domain::event::{EventKind, FactoryEvent};
use serde_json::json;

use super::alert_for;

fn pr_feedback(action: &str) -> FactoryEvent {
    let payload =
        json!({"action": action, "round": 2, "pr": 42, "why": "max_pr_rework_rounds reached"});
    FactoryEvent::new(EventKind::PrFeedback, payload.to_string(), 7).for_task("t1")
}

#[test]
fn pr_feedback_exhausted_is_a_needs_you_alert() {
    let alert = alert_for(&pr_feedback("exhausted"), &|_| "fix it".into()).unwrap();
    assert_eq!(alert.kind, "pr_exhausted");
    assert!(alert.text.contains("t1 (fix it)"), "{}", alert.text);
    assert!(alert.text.contains("#42"), "{}", alert.text);
}

#[test]
fn pr_feedback_rework_and_skipped_are_not_alerts() {
    for action in ["rework", "skipped"] {
        assert!(alert_for(&pr_feedback(action), &|_| String::new()).is_none());
    }
}

#[test]
fn infra_block_with_a_scheduled_retry_is_silent_but_the_final_one_alerts() {
    let ev = |payload: serde_json::Value| {
        FactoryEvent::new(EventKind::TaskStateChanged, payload.to_string(), 7).for_task("t1")
    };
    let silent = ev(json!({"to": "blocked", "why": "infra", "auto_retry_at": 99, "auto_retry": 1}));
    assert!(alert_for(&silent, &|_| String::new()).is_none());
    let last = ev(json!({"to": "blocked", "why": "infra; 6 automatic retries used"}));
    assert_eq!(
        alert_for(&last, &|_| String::new()).unwrap().kind,
        "blocked"
    );
}

#[test]
fn merge_retries_used_up_is_a_needs_you_alert_but_a_pending_block_is_not() {
    let ev = |code: &str| {
        let payload =
            json!({"merged": false, "kind": "pr_merge", "code": code, "url": "https://x/pull/9"});
        FactoryEvent::new(EventKind::MergeResult, payload.to_string(), 7).for_task("t1")
    };
    let alert = alert_for(&ev("merge_retry_exhausted"), &|_| "t".into()).unwrap();
    assert_eq!(alert.kind, "merge_stuck");
    assert!(alert.text.contains("https://x/pull/9"), "{}", alert.text);
    assert!(alert_for(&ev("ci_pending"), &|_| String::new()).is_none());
}
