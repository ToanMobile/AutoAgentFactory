#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Oracle test for AUTO-024: stream-json agent events (`result`, `user`,
//! `assistant`) quoting repo test errors that contain HTTP 5xx / 529 substrings
//! must NOT be misclassified as `BlockedReason::Infra`.

use aaf_domain::blocked::{BlockedReason, UnblockTier};
use aaf_domain::config::UnblockerConfig;
use aaf_domain::ports::{AgentOutcome, Clock, TaskStore};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::executors::claude::parse_stream;
use aaf_engine::unblocker::{FailureSignal, Ladder, classify};

struct FixedClock(i64);

impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0
    }
}

fn failed_outcome(log_tail: &str) -> AgentOutcome {
    AgentOutcome {
        exit_code: Some(1),
        timed_out: false,
        final_text: String::new(),
        tokens_in: 0,
        tokens_out: 0,
        limit_hit: None,
        infra: None,
        log_tail: log_tail.to_string(),
    }
}

fn signal_from_log_tail(log_tail: &str) -> FailureSignal {
    let mut signal = FailureSignal::empty();
    signal.outcome = Some(failed_outcome(log_tail));
    signal
}

#[test]
fn stream_json_result_event_quoting_5xx_classifies_as_stuck_not_infra() {
    let line = r#"{"type":"result","is_error":true,"result":"cargo test failed: HTTP 503 overloaded server_error"}"#;
    assert_eq!(parse_stream(line).infra, None);

    let reason = classify(&signal_from_log_tail(line));
    assert!(
        matches!(reason, BlockedReason::Stuck { .. }),
        "stream-json `result` event quoting test error must classify as Stuck, not Infra; got: {reason:?}"
    );
}

#[test]
fn stream_json_user_tool_result_quoting_5xx_classifies_as_stuck_not_infra() {
    let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"expected status 200, got status 500"}]}}"#;
    assert_eq!(parse_stream(line).infra, None);

    let reason = classify(&signal_from_log_tail(line));
    assert!(
        matches!(reason, BlockedReason::Stuck { .. }),
        "stream-json `user` tool_result quoting status 500 must classify as Stuck, not Infra; got: {reason:?}"
    );
}

#[test]
fn stream_json_assistant_event_quoting_5xx_classifies_as_stuck_not_infra() {
    let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Seeing 503 service unavailable in unit test mock"}]}}"#;
    assert_eq!(parse_stream(line).infra, None);

    let reason = classify(&signal_from_log_tail(line));
    assert!(
        matches!(reason, BlockedReason::Stuck { .. }),
        "stream-json `assistant` event quoting 503 must classify as Stuck, not Infra; got: {reason:?}"
    );
}

#[test]
fn stream_json_agent_events_enter_t3_fresh_context_instead_of_infra_retry() {
    let log_tail = [
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Seeing 503 service unavailable in unit test mock"}]}}"#,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"expected status 200, got status 500"}]}}"#,
        r#"{"type":"result","is_error":true,"result":"cargo test failed: HTTP 503 overloaded server_error"}"#,
    ]
    .join("\n");
    assert_eq!(parse_stream(&log_tail).infra, None);

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("state.db")).unwrap();
    let cfg = UnblockerConfig::default();
    let clock = FixedClock(1_000);
    let spec = ImmutableSpec {
        description: "desc".to_string(),
        acceptance: vec!["ok".to_string()],
    };
    let mut task = Task::new_root("t-auto024", "/repo", "task", spec, Priority::P2, 100);
    task.state = TaskState::Running;
    db.insert_task(&task).unwrap();

    let ladder = Ladder::new(&db, &db, &db, &db, &clock, &cfg);
    let signal = signal_from_log_tail(&log_tail);
    let decision = ladder.next_action("t-auto024", &signal, None).unwrap();

    assert!(
        matches!(decision.reason, BlockedReason::Stuck { .. }),
        "expected BlockedReason::Stuck for stream-json test failure, got: {:?}",
        decision.reason
    );
    assert_eq!(
        decision.tier,
        UnblockTier::T3FreshContext,
        "expected T3FreshContext recovery instead of skipping T3 via Infra retry; decision: {decision:?}"
    );
}

#[test]
fn plain_text_non_stream_json_infra_errors_still_classify_as_infra() {
    for (label, tail) in [
        ("plain HTTP 503", "Error: HTTP 503 Service Unavailable"),
        (
            "plain status 529",
            "upstream request failed with status 529",
        ),
    ] {
        let reason = classify(&signal_from_log_tail(tail));
        assert!(
            matches!(reason, BlockedReason::Infra { .. }),
            "plain-text infra case `{label}` must still classify as Infra; got: {reason:?}"
        );
    }
}
