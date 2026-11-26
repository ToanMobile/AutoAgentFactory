#![allow(clippy::unwrap_used, clippy::expect_used)]

use aaf_domain::blocked::{BlockedReason, UnblockAction, UnblockTier};
use aaf_domain::config::{
    CrossAuditConfig, DEFAULT_CROSS_AUDIT_MAX_DIFF_BYTES, MAX_CROSS_AUDIT_MAX_DIFF_BYTES,
    MIN_CROSS_AUDIT_MAX_DIFF_BYTES, UnblockerConfig,
};
use aaf_domain::ports::{AgentOutcome, Clock, TaskStore};
use aaf_domain::task::{ImmutableSpec, Priority, Task};
use aaf_engine::cross_audit::prompt::{
    MAX_DIFF_BYTES, top_diff_files, truncate_diff, truncate_diff_with_limit,
};
use aaf_engine::db::Db;
use aaf_engine::unblocker::{FailureSignal, Ladder, classify};

struct FixedClock(i64);
impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0
    }
}

#[test]
fn default_cross_audit_max_diff_bytes_handles_225kb_diffs_without_truncation() {
    assert_eq!(DEFAULT_CROSS_AUDIT_MAX_DIFF_BYTES, 400_000);
    assert_eq!(MAX_DIFF_BYTES, 400_000);
    assert_eq!(CrossAuditConfig::default().max_diff_bytes, 400_000);
    const { assert!(MIN_CROSS_AUDIT_MAX_DIFF_BYTES < DEFAULT_CROSS_AUDIT_MAX_DIFF_BYTES) };
    const { assert!(DEFAULT_CROSS_AUDIT_MAX_DIFF_BYTES < MAX_CROSS_AUDIT_MAX_DIFF_BYTES) };

    let diff_230k = "a".repeat(230_000);
    assert!(!truncate_diff(&diff_230k).1);
    assert!(truncate_diff_with_limit(&diff_230k, 200_000).1);
}

#[test]
fn top_diff_files_orders_largest_sections_first() {
    let small = format!("diff --git a/small.rs b/small.rs\n+{}\n", "x".repeat(500));
    let big = format!("diff --git a/big.kt b/big.kt\n+{}\n", "y".repeat(9000));
    let summary = top_diff_files(&format!("{small}{big}"));
    assert!(summary.starts_with("big.kt ("), "got: {summary}");
    assert!(summary.contains("small.rs ("), "got: {summary}");
}

#[test]
fn oversized_diff_signal_skips_t3_retry_and_routes_directly_to_t4_or_t5() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    let task = Task::new_root(
        "t-big",
        "/repo/a",
        "Huge PR",
        ImmutableSpec {
            description: "spec".into(),
            acceptance: vec!["ac".into()],
        },
        Priority::P1,
        100,
    );
    db.insert_task(&task).unwrap();

    let mut signal = FailureSignal::empty();
    signal.outcome = Some(AgentOutcome {
        exit_code: Some(1),
        timed_out: false,
        final_text: String::new(),
        tokens_in: 0,
        tokens_out: 0,
        limit_hit: None,
        infra: None,
        log_tail: "audit_diff_truncated: inconclusive: diff was truncated (500000 > 400000 bytes)"
            .into(),
    });
    let reason = classify(&signal);
    assert!(
        matches!(&reason, BlockedReason::Stuck { detail } if detail.starts_with("diff_too_large:")),
        "expected diff_too_large Stuck reason, got {reason:?}"
    );

    let clock = FixedClock(200);
    let cfg = UnblockerConfig::default();
    let ladder = Ladder::new(&db, &db, &db, &db, &clock, &cfg);
    // Without a proposer, skipping T3 goes straight to T4/T5 escalation instead of T3 RetryFreshContext.
    let decision = ladder.next_action("t-big", &signal, None).unwrap();
    assert_ne!(
        decision.tier,
        UnblockTier::T3FreshContext,
        "oversized diff must skip pointless T3 fresh-context retry"
    );
    assert!(
        !matches!(decision.action, UnblockAction::RetryFreshContext { .. }),
        "unexpected T3 retry: {:?}",
        decision.action
    );
}
