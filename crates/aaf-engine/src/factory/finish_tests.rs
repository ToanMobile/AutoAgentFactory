use super::receipt_duration_ms;
use aaf_domain::gate::{
    CommandResult, GateMode, GateStatus, HeadLockReport, LockStatus, RECEIPT_SCHEMA_VERSION,
    Receipt, StoreComplianceReport,
};

fn receipt_with_durations(durations: &[u64]) -> Receipt {
    Receipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        repo_root: "/repo".to_string(),
        tree_fp: "fp".to_string(),
        head_sha: None,
        base_sha: None,
        mode: GateMode::Scoped,
        status: GateStatus::Clear,
        reasons: Vec::new(),
        config_hash: "cfg".to_string(),
        commands: durations
            .iter()
            .enumerate()
            .map(|(i, &duration_ms)| CommandResult {
                name: format!("c{i}"),
                command: "true".to_string(),
                exit_code: Some(0),
                timed_out: false,
                duration_ms,
                log_tail: String::new(),
            })
            .collect(),
        static_findings: Vec::new(),
        suppressions: 0,
        head_lock: HeadLockReport {
            status: LockStatus::Clear,
            lock_fp: String::new(),
            locked_changes: Vec::new(),
            approval: None,
        },
        store_compliance: StoreComplianceReport::not_applicable(),
        created_at: 0,
    }
}

#[test]
fn receipt_duration_ms_sums_and_saturates() {
    assert_eq!(receipt_duration_ms(&receipt_with_durations(&[])), 0);
    assert_eq!(
        receipt_duration_ms(&receipt_with_durations(&[450, 750, 600])),
        1800
    );
    assert_eq!(
        receipt_duration_ms(&receipt_with_durations(&[u64::MAX, 10])),
        u64::MAX
    );
}
