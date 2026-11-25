//! `alert_for` mapping of PR feedback, factory-freeze and cost-cap events.

use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_engine::telegram::MAX_MESSAGE_BYTES;
use serde_json::{Value, json};

use super::deliver::{format_telegram_alert, parse_cockpit_url};
use super::{alert_for, fmt_utc};

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

fn event(kind: EventKind, payload: Value, task_id: Option<&str>) -> FactoryEvent {
    let mut ev = FactoryEvent::new(kind, payload.to_string(), 1_700_000_000);
    ev.id = Some(42);
    if let Some(id) = task_id {
        ev = ev.for_task(id);
    }
    ev
}

#[test]
fn fmt_utc_formats_epoch_and_leap_years() {
    assert_eq!(fmt_utc(0), "1970-01-01 00:00 UTC");
    assert_eq!(fmt_utc(951_827_400), "2000-02-29 12:30 UTC");
    assert_eq!(fmt_utc(1_709_251_140), "2024-02-29 23:59 UTC");
    assert_eq!(fmt_utc(1_709_251_200), "2024-03-01 00:00 UTC");
}

#[test]
fn alert_for_factory_frozen_formats_token_spike_and_preserves_legacy_text() {
    let spike = event(
        EventKind::FactoryFrozen,
        json!({
            "reason": "token_spike",
            "tokens_last_hour": 5000,
            "limit_last_hour": 4000,
            "consecutive_fails": 0,
            "threshold": 0
        }),
        None,
    );
    let alert = alert_for(&spike, &|_| String::new()).unwrap();
    assert_eq!(alert.kind, "frozen");
    assert_eq!(
        alert.text,
        "aaf: factory frozen: token spike 5000 tokens in the last hour (limit 4000); \
         check for a runaway agent, then `aaf board unfreeze`"
    );

    let legacy = event(
        EventKind::FactoryFrozen,
        json!({ "root_id": "t-9", "consecutive_fails": 3, "threshold": 3 }),
        Some("t-9"),
    );
    let legacy_alert = alert_for(&legacy, &|id| format!("Title {id}")).unwrap();
    assert_eq!(legacy_alert.kind, "frozen");
    assert_eq!(
        legacy_alert.text,
        "aaf: factory frozen after 3 failed root task(s) in a row (last: t-9 (Title t-9)); \
         nothing starts until `aaf board unfreeze`"
    );
}

#[test]
fn alert_for_cost_paused_and_cost_resumed() {
    let paused = event(
        EventKind::CostPaused,
        json!({
            "tokens_24h": 1200,
            "cap": 1000,
            "resume_at": 1_709_251_140
        }),
        None,
    );
    let paused_alert = alert_for(&paused, &|_| String::new()).unwrap();
    assert_eq!(paused_alert.kind, "paused");
    assert_eq!(
        paused_alert.text,
        "aaf: daily token cap reached (1200/1000 in 24h); \
         new work resumes automatically around 2024-02-29 23:59 UTC"
    );

    let resumed = event(
        EventKind::CostResumed,
        json!({
            "tokens_24h": 0,
            "cap": 1000
        }),
        None,
    );
    let resumed_alert = alert_for(&resumed, &|_| String::new()).unwrap();
    assert_eq!(resumed_alert.kind, "resumed");
    assert_eq!(
        resumed_alert.text,
        "aaf: daily token cap window cleared (0/1000 in 24h); new work resumes"
    );
}

#[test]
fn format_telegram_alert_appends_task_deep_link() {
    let out = format_telegram_alert(
        "aaf: t-abc (Fix login) needs you: gate failed",
        Some("https://mac.tailnet.ts.net"),
        Some("t-abc"),
    );
    assert_eq!(
        out,
        "aaf: t-abc (Fix login) needs you: gate failed\nCockpit: https://mac.tailnet.ts.net/?task=t-abc"
    );
    assert!(!out.contains("#token="));
}

#[test]
fn format_telegram_alert_appends_needyou_without_task_id() {
    let base = "https://mac.tailnet.ts.net";
    for task_id in [None, Some(""), Some("   ")] {
        let out = format_telegram_alert("aaf: factory frozen", Some(base), task_id);
        assert_eq!(
            out,
            "aaf: factory frozen\nCockpit: https://mac.tailnet.ts.net/?view=needyou"
        );
        assert!(!out.contains("#token="));
    }
}

#[test]
fn format_telegram_alert_rejects_malicious_task_id_and_falls_back_to_needyou() {
    let base = "https://mac.tailnet.ts.net";
    let too_long = "a".repeat(129);
    for bad_id in [
        "../bad&x=1",
        "t-1#token=leak",
        "t-1?extra=1",
        "t-1/../../etc",
        "t-1 with spaces",
        too_long.as_str(),
    ] {
        let out = format_telegram_alert("aaf: needs you", Some(base), Some(bad_id));
        assert_eq!(
            out, "aaf: needs you\nCockpit: https://mac.tailnet.ts.net/?view=needyou",
            "invalid task_id {bad_id:?} must not be interpolated into ?task="
        );
        assert!(!out.contains("?task="));
    }
}

#[test]
fn format_telegram_alert_unchanged_when_cockpit_url_is_none() {
    let raw = "aaf: t-abc (Fix login) is ready for review";
    let out = format_telegram_alert(raw, None, Some("t-abc"));
    assert_eq!(out, raw);
    assert!(!out.contains("Cockpit:"));
    assert!(!out.contains("#token="));
}

#[test]
fn format_telegram_alert_truncates_utf8_body_and_preserves_cockpit_link() {
    // 4500+ bytes containing multi-byte Vietnamese UTF-8 characters.
    let unit = "Cảnh báo lỗi hệ thống: tiếng Việt có dấu ệ ở ứ — ";
    let repeats = (4500 / unit.len()) + 2;
    let body = unit.repeat(repeats);
    assert!(body.len() > 4500, "body should exceed 4500 bytes");

    let expected_suffix = "\nCockpit: https://mac.tailnet.ts.net/?task=t-abc";
    let out = format_telegram_alert(&body, Some("https://mac.tailnet.ts.net"), Some("t-abc"));
    assert!(
        out.len() <= MAX_MESSAGE_BYTES,
        "expected <= {MAX_MESSAGE_BYTES} bytes, got {}",
        out.len()
    );
    assert!(
        out.ends_with(&format!("\n...(truncated){expected_suffix}")),
        "deep link must remain intact after truncation"
    );
    assert!(!out.contains("#token="));

    let no_task = format_telegram_alert(&body, Some("https://mac.tailnet.ts.net"), None);
    assert!(no_task.len() <= MAX_MESSAGE_BYTES);
    assert!(
        no_task.ends_with("\n...(truncated)\nCockpit: https://mac.tailnet.ts.net/?view=needyou")
    );
    assert!(!no_task.contains("#token="));
}

#[test]
fn parse_cockpit_url_accepts_canonical_https_and_rejects_invalid() {
    assert_eq!(
        parse_cockpit_url("  https://mac.tailnet.ts.net/ "),
        Some("https://mac.tailnet.ts.net".to_string())
    );
    assert_eq!(
        parse_cockpit_url("https://mac.tailnet.ts.net:8443"),
        Some("https://mac.tailnet.ts.net:8443".to_string())
    );
    assert_eq!(parse_cockpit_url(""), None);
    assert_eq!(parse_cockpit_url("   "), None);
    assert_eq!(parse_cockpit_url("http://mac.tailnet.ts.net"), None);
    assert_eq!(
        parse_cockpit_url("https://mac.tailnet.ts.net/#token=secret"),
        None
    );
    assert_eq!(parse_cockpit_url("https://mac.tailnet.ts.net/path"), None);
}
