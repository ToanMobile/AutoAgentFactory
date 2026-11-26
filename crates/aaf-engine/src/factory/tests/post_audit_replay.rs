//! Regression tests for crash-replay post-gate `CrossAudit` FAIL detection and
//! rework cap accounting (`reused_from` must not double-count failures).

use aaf_domain::config::CrossAuditConfig;
use aaf_domain::event::EventKind;
use aaf_domain::gate::{
    GateMode, GateStatus, HeadLockReport, LockStatus, Receipt, StoreComplianceReport,
};
use aaf_domain::ports::TaskStore;
use aaf_domain::task::TaskState;
use serde_json::json;

use super::world::World;
use crate::cross_audit::Finding;
use crate::factory::Deps;
use crate::factory::post_audit::find_interrupted_post_gate_fail;
use crate::factory::unblock::audit_fails_since_reset;

fn sample_receipt(tree_fp: &str, config_hash: &str) -> Receipt {
    Receipt {
        schema_version: 2,
        repo_root: "/repo".to_string(),
        tree_fp: tree_fp.to_string(),
        base_sha: Some("abc123".to_string()),
        head_sha: Some("def456".to_string()),
        config_hash: config_hash.to_string(),
        mode: GateMode::Scoped,
        status: GateStatus::Clear,
        commands: Vec::new(),
        head_lock: HeadLockReport {
            status: LockStatus::Clear,
            lock_fp: String::new(),
            locked_changes: Vec::new(),
            approval: None,
        },
        static_findings: Vec::new(),
        suppressions: 0,
        store_compliance: StoreComplianceReport::not_applicable(),
        reasons: Vec::new(),
        created_at: 10,
    }
}

fn emit_event(deps: &Deps, task_id: &str, kind: EventKind, payload: serde_json::Value) {
    deps.emit(kind, task_id, payload).unwrap();
}

#[test]
fn interrupted_post_gate_fail_is_reused_without_double_counting_cap() {
    let world = World::new();
    world.add("t1");
    world
        .db
        .set_state("t1", TaskState::Running, None, 1)
        .unwrap();
    world
        .db
        .set_state("t1", TaskState::Gating, None, 2)
        .unwrap();
    let deps = world.deps();
    emit_event(
        &deps,
        "t1",
        EventKind::TaskStateChanged,
        json!({ "to": "gating", "why": "agent run finished" }),
    );
    let saved = vec![Finding {
        id: "BUG".to_string(),
        question: "missing null check?".to_string(),
        evidence: "src/foo.rs:42 derefs unchecked".to_string(),
    }];
    emit_event(
        &deps,
        "t1",
        EventKind::CrossAudit,
        json!({
            "phase": "post_gate",
            "verdict": "fail: BUG",
            "tree_fp": "tree-1",
            "config_hash": "cfg-1",
            "retry": false,
            "findings": saved,
        }),
    );

    let cfg = CrossAuditConfig::default();
    let receipt = sample_receipt("tree-1", "cfg-1");

    let found = find_interrupted_post_gate_fail(&deps, "t1", &receipt, &cfg)
        .unwrap()
        .expect("should reuse interrupted fail from current gating visit");
    assert_eq!(found.verdict, "fail: BUG");
    assert_eq!(found.findings, saved);

    // Emit the crash-replay audit event carrying `reused_from`: cap count stays 1, not 2.
    emit_event(
        &deps,
        "t1",
        EventKind::CrossAudit,
        json!({
            "phase": "post_gate",
            "verdict": found.verdict,
            "tree_fp": "tree-1",
            "config_hash": "cfg-1",
            "reused_from": found.event_id,
            "findings": found.findings,
        }),
    );
    assert_eq!(audit_fails_since_reset(&deps, "t1").unwrap(), 1);
}

#[test]
fn post_gate_fail_is_not_reused_after_state_transition_or_mismatch_or_empty_findings() {
    let world = World::new();
    world.add("t2");
    let deps = world.deps();
    let cfg = CrossAuditConfig::default();
    let receipt = sample_receipt("tree-a", "cfg-a");

    // Missing structured findings -> never reuse (zero quality loss).
    emit_event(
        &deps,
        "t2",
        EventKind::CrossAudit,
        json!({
            "phase": "post_gate",
            "verdict": "fail: missing details",
            "tree_fp": "tree-a",
            "config_hash": "cfg-a",
            "retry": false,
        }),
    );
    assert!(
        find_interrupted_post_gate_fail(&deps, "t2", &receipt, &cfg)
            .unwrap()
            .is_none()
    );

    // Valid findings, but different tree_fp or config_hash -> no reuse.
    emit_event(
        &deps,
        "t2",
        EventKind::CrossAudit,
        json!({
            "phase": "post_gate",
            "verdict": "fail: BUG",
            "tree_fp": "tree-a",
            "config_hash": "cfg-a",
            "retry": false,
            "findings": [{ "id": "BUG", "question": "q", "evidence": "src/a.rs:10" }],
        }),
    );
    assert!(
        find_interrupted_post_gate_fail(&deps, "t2", &sample_receipt("tree-b", "cfg-a"), &cfg)
            .unwrap()
            .is_none()
    );
    assert!(
        find_interrupted_post_gate_fail(&deps, "t2", &sample_receipt("tree-a", "cfg-b"), &cfg)
            .unwrap()
            .is_none()
    );

    // Once routing moved the task out of Gating (e.g. rework / human unblock), never reuse.
    emit_event(
        &deps,
        "t2",
        EventKind::TaskStateChanged,
        json!({ "to": "ready", "why": "audit_rework" }),
    );
    assert!(
        find_interrupted_post_gate_fail(&deps, "t2", &receipt, &cfg)
            .unwrap()
            .is_none()
    );
}
