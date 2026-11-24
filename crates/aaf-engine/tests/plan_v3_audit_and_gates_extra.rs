#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Plan v3 Wave 2 Slice 4 tests (part 2 — split from `plan_v3_audit_and_gates.rs`):
//! - Scoped `store_compliance` filtering & mechanical scoring (`GATE-04`, `GATE-05`)
//! - Expanded `extract_gate_diagnostics` (3000 chars, store findings, static fix hints)
//! - Progress-aware `T3FreshContext` (max 2 retries when errors shrink)
//! - Unbounded SQL `audit_fails_since_reset` (>60 intervening events)

use std::sync::atomic::{AtomicI64, Ordering};

use aaf_domain::blocked::{UnblockAction, UnblockTier};
use aaf_domain::config::UnblockerConfig;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::gate::{
    CommandResult, GateMode, GateStatus, HeadLockReport, LockStatus, RECEIPT_SCHEMA_VERSION,
    Receipt, Severity, StaticFinding, StoreComplianceReport, StoreFinding, StoreStatus,
};
use aaf_domain::ports::{Clock, EventSink, TaskStore};
use aaf_domain::task::{ImmutableSpec, Priority, Task};
use aaf_engine::cross_audit::compute_mechanical_score;
use aaf_engine::db::Db;
use aaf_engine::store_compliance::{PRE_EXISTING_PREFIX, scope_to_diff};
use aaf_engine::unblocker::Ladder;
use aaf_engine::unblocker::classifier::{
    FailureSignal, GATE_DIAG_MAX_CHARS, extract_gate_diagnostics,
};
use tempfile::TempDir;

struct TestClock(AtomicI64);
impl Clock for TestClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

fn base_receipt(repo_root: &str, mode: GateMode, status: GateStatus, cfg_hash: &str) -> Receipt {
    Receipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        repo_root: repo_root.into(),
        tree_fp: "git-tree:t1".into(),
        head_sha: Some("head1".into()),
        base_sha: Some("base0".into()),
        mode,
        status,
        reasons: vec![],
        config_hash: cfg_hash.into(),
        commands: vec![],
        static_findings: vec![],
        suppressions: 0,
        head_lock: HeadLockReport {
            status: LockStatus::Clear,
            lock_fp: String::new(),
            locked_changes: vec![],
            approval: None,
        },
        store_compliance: StoreComplianceReport::not_applicable(),
        created_at: 2000,
    }
}

fn sample_spec() -> ImmutableSpec {
    ImmutableSpec {
        description: "Add parser module".into(),
        acceptance: vec!["Parses valid input".into()],
    }
}

#[test]
fn scoped_store_compliance_ignores_pre_existing_debt_and_blocks_diff_criticals() {
    let finding = StoreFinding {
        rule: "AND-001".into(),
        severity: Severity::Critical,
        platform: "android".into(),
        guideline: "Play policy".into(),
        file: "app/src/main/AndroidManifest.xml".into(),
        line: Some(12),
        message: "debuggable enabled".into(),
        fix: "set debuggable=false".into(),
    };
    let full = StoreComplianceReport::from_findings(
        vec![finding],
        vec!["AND-003 app/build.gradle:10: dynamic targetSdk".into()],
    );
    assert_eq!(full.status, StoreStatus::Blocked);

    let scoped_unrelated = scope_to_diff(full.clone(), &["src/feature.rs".into()]);
    assert_eq!(scoped_unrelated.status, StoreStatus::Clear);
    assert_eq!(
        (scoped_unrelated.critical, scoped_unrelated.warnings),
        (0, 1)
    );
    assert!(
        scoped_unrelated.findings[0]
            .message
            .starts_with(PRE_EXISTING_PREFIX)
    );
    assert!(scoped_unrelated.unresolved[0].starts_with(PRE_EXISTING_PREFIX));

    let mut receipt = base_receipt("/repo", GateMode::Scoped, GateStatus::Clear, "cfg");
    receipt.store_compliance = scoped_unrelated;
    assert_eq!(
        compute_mechanical_score(&receipt, true, false, false).score,
        100
    );
    assert_eq!(
        compute_mechanical_score(&receipt, true, true, true).score,
        94
    );
    receipt.mode = GateMode::Full;
    assert!(compute_mechanical_score(&receipt, true, false, false).score < 100);

    let scoped_touched = scope_to_diff(full, &["app/src/main/AndroidManifest.xml".into()]);
    assert_eq!(scoped_touched.status, StoreStatus::Blocked);
    assert_eq!(scoped_touched.critical, 1);
}

#[test]
fn extract_gate_diagnostics_includes_store_findings_static_fix_and_reasons() {
    assert_eq!(GATE_DIAG_MAX_CHARS, 3000);
    let mut receipt = base_receipt("/repo", GateMode::Scoped, GateStatus::Blocked, "cfg");
    receipt.reasons = vec![
        "oracle: vacuous test — tests still pass".into(),
        "proof required: missing screenshot".into(),
        "work tree: hidden file .env.local".into(),
    ];
    receipt.commands = vec![CommandResult {
        name: "test_scoped".into(),
        command: "cargo test".into(),
        exit_code: Some(1),
        timed_out: false,
        duration_ms: 10,
        log_tail: "test result: FAILED. 0 passed; 2 failed;".into(),
    }];
    receipt.static_findings = vec![StaticFinding {
        rule: "SEC-001".into(),
        severity: Severity::Critical,
        file: "src/auth.rs".into(),
        line: Some(19),
        message: "hardcoded token".into(),
        fix: "load secret from env var".into(),
    }];
    receipt.store_compliance = StoreComplianceReport::from_findings(
        vec![StoreFinding {
            rule: "AND-002".into(),
            severity: Severity::Critical,
            platform: "android".into(),
            guideline: "Android 12 exported".into(),
            file: "AndroidManifest.xml".into(),
            line: Some(44),
            message: "activity missing android:exported".into(),
            fix: "add android:exported=\"false\"".into(),
        }],
        vec![],
    );
    let mut signal = FailureSignal::empty();
    signal.gate = Some(receipt);
    let diag = extract_gate_diagnostics(&signal).expect("expected diagnostics");
    assert!(diag.contains("SEC-001 @ src/auth.rs:19"));
    assert!(diag.contains("load secret from env var"));
    assert!(diag.contains("AND-002 @ AndroidManifest.xml:44"));
    assert!(diag.contains("add android:exported=\"false\""));
    assert!(diag.contains("oracle: vacuous test"));
    assert!(diag.contains("proof required: missing screenshot"));
    assert!(diag.contains("work tree: hidden file .env.local"));
}

#[test]
fn ladder_allows_second_t3_attempt_only_when_error_count_shrinks() {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    let clock = TestClock(AtomicI64::new(1000));
    db.insert_task(&Task::new_root(
        "t1",
        "/repo",
        "Fix tests",
        sample_spec(),
        Priority::P2,
        100,
    ))
    .unwrap();
    let cfg = UnblockerConfig::default();
    let ladder = Ladder::new(&db, &db, &db, &db, &clock, &cfg);

    let failed_signal = |failed_n: u32| {
        let mut r = base_receipt("/repo", GateMode::Scoped, GateStatus::Blocked, "cfg");
        r.reasons = vec!["command test_scoped failed (exit 1)".into()];
        r.commands = vec![CommandResult {
            name: "test_scoped".into(),
            command: "cargo test".into(),
            exit_code: Some(1),
            timed_out: false,
            duration_ms: 10,
            log_tail: format!("test result: FAILED. 1 passed; {failed_n} failed;"),
        }];
        let mut s = FailureSignal::empty();
        s.gate = Some(r);
        s
    };

    let d1 = ladder.next_action("t1", &failed_signal(5), None).unwrap();
    assert_eq!(d1.tier, UnblockTier::T3FreshContext);
    assert!(matches!(d1.action, UnblockAction::RetryFreshContext { .. }));
    let d2 = ladder.next_action("t1", &failed_signal(2), None).unwrap();
    assert_eq!(d2.tier, UnblockTier::T3FreshContext);
    let d3 = ladder.next_action("t1", &failed_signal(1), None).unwrap();
    assert_eq!(d3.tier, UnblockTier::T5Escalate);
}

#[test]
fn audit_fails_since_reset_is_accurate_beyond_60_intervening_events() {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    db.insert_task(&Task::new_root(
        "t1",
        "/repo",
        "Long history",
        sample_spec(),
        Priority::P2,
        100,
    ))
    .unwrap();

    for i in 0..2 {
        db.emit(
            &FactoryEvent::new(
                EventKind::CrossAudit,
                serde_json::json!({ "verdict": "fail: STUB (src/lib.rs:1)" }).to_string(),
                100 + i,
            )
            .for_task("t1"),
        )
        .unwrap();
    }
    for i in 0..65 {
        db.emit(&FactoryEvent::new(EventKind::GateResult, "{}", 200 + i).for_task("t1"))
            .unwrap();
    }
    db.emit(
        &FactoryEvent::new(
            EventKind::CrossAudit,
            serde_json::json!({ "verdict": "fail: ASSERT (src/lib.rs:2)" }).to_string(),
            400,
        )
        .for_task("t1"),
    )
    .unwrap();
    assert_eq!(db.audit_fails_since_reset("t1").unwrap(), 3);

    db.emit(
        &FactoryEvent::new(
            EventKind::TaskStateChanged,
            serde_json::json!({ "to": "ready", "why": "unblocked" }).to_string(),
            500,
        )
        .for_task("t1"),
    )
    .unwrap();
    assert_eq!(db.audit_fails_since_reset("t1").unwrap(), 0);
}
