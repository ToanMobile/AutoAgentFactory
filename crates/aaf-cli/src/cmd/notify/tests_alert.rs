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
