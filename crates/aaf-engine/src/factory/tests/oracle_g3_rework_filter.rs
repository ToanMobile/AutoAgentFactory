//! Oracle test for G3: filter unverified / evidenceless audit findings out of
//! rework notes. These tests MUST fail on base `92367ec` and pass only after
//! the fix is applied. NEVER edit this file after Phase 0.

use aaf_domain::ports::{BudgetStore, TaskStore};
use aaf_domain::task::TaskState;

use super::world::World;
use crate::cross_audit::Finding;
use crate::factory::unblock::handle_audit_failure;

/// A finding with concrete file:line evidence — should survive the filter.
fn valid_finding() -> Finding {
    Finding {
        id: "ASSERT".to_string(),
        question: "Does the diff delete or weaken an assertion?".to_string(),
        evidence: "src/lib.rs:10 assertion removed".to_string(),
    }
}

/// A finding whose evidence is empty — must be dropped by the filter.
fn invalid_finding() -> Finding {
    Finding {
        id: "HARDCODED".to_string(),
        question: "Does the diff hardcode a value to make a test pass?".to_string(),
        evidence: String::new(),
    }
}

fn add_gating_task(world: &World, id: &str) -> aaf_domain::task::Task {
    world.add(id);
    world.db.set_state(id, TaskState::Running, None, 1).unwrap();
    world.db.set_state(id, TaskState::Gating, None, 2).unwrap();
    world.db.get_task(id).unwrap()
}

// ---------------------------------------------------------------------------
// Scenario A: mixed — one valid + one invalid finding.
// Expected (after fix):
//   (a) the empty-evidence finding does NOT appear in audit_rework note
//   (b) the valid finding DOES appear
// ---------------------------------------------------------------------------
#[test]
fn empty_evidence_finding_is_excluded_from_rework_note() {
    let world = World::new();
    let task = add_gating_task(&world, "t1");
    let deps = world.deps();

    let findings = vec![valid_finding(), invalid_finding()];
    let result = handle_audit_failure(&deps, &task, &findings).unwrap();

    // Must route back to Ready (at least one valid finding).
    assert!(result.is_some(), "expected Some(Next::Ready), got None");
    assert_eq!(world.state("t1"), TaskState::Ready);

    let notes = world.task("t1").context_notes;
    let rework_note = notes
        .iter()
        .find(|n| n.starts_with("audit_rework:"))
        .unwrap();

    // (b) valid finding appears
    assert!(
        rework_note.contains("src/lib.rs:10"),
        "valid finding evidence must appear in rework note: {rework_note}"
    );
    // (a) invalid finding must NOT appear — the empty-evidence HARDCODED entry
    assert!(
        !rework_note.contains("HARDCODED"),
        "empty-evidence finding must NOT appear in rework note: {rework_note}"
    );
}

// ---------------------------------------------------------------------------
// Scenario B: ALL findings have empty evidence.
// Expected (after fix):
//   (c) handle_audit_failure returns Ok(None) WITHOUT charging a budget cycle
//       or moving the task to Ready.
// ---------------------------------------------------------------------------
#[test]
fn all_invalid_findings_returns_none_and_no_budget_charged() {
    let world = World::new();
    let task = add_gating_task(&world, "t1");
    let deps = world.deps();

    let findings = vec![invalid_finding()];
    let result = handle_audit_failure(&deps, &task, &findings).unwrap();

    // (c) must return None — no routing to author
    assert!(
        result.is_none(),
        "all-invalid findings must return Ok(None), got Some(_)"
    );

    // task must NOT be moved to Ready
    assert_ne!(
        world.state("t1"),
        TaskState::Ready,
        "task must NOT be reset to Ready when all findings are invalid"
    );

    // budget cycle must NOT be charged — the budget row may or may not exist,
    // but cycles_used must remain 0.
    if let Ok(budget) = world.db.get_budget("t1") {
        assert_eq!(
            budget.cycles_used, 0,
            "no budget cycle must be charged when all findings are invalid"
        );
    }
}
