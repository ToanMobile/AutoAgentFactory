//! `ReviewInputs` of `run_reviewer_turn`: the factory's own dual review passes
//! the values it always used (`ReviewInputs::current`), `aaf board adopt`
//! passes its own policy, diff base and proof verdict.

use std::sync::Arc;

use aaf_domain::event::EventKind;
use aaf_domain::gate::{CommandResult, GateStatus, HeadLockReport, LockStatus, Receipt};
use aaf_domain::pipeline::ReviewerConfig;
use aaf_domain::ports::EventSink;

use super::fakes::receipt;
use super::fakes_git::worktree;
use super::world::World;
use crate::factory::Grant;
use crate::factory::dual_audit::{
    PartialTurnResult, ReviewInputs, dual_mech_score_with, run_reviewer_turn,
};
use crate::factory::stage_router::StageTarget;
use crate::factory::worker::GateJob;

const BASE: &str = "0123456789abcdef0123456789abcdef01234567";

fn job() -> GateJob {
    GateJob {
        task_id: "t1".to_string(),
        worktree: worktree("t1"),
        audited: false,
        cached_receipt: None,
    }
}

fn target(world: &World) -> StageTarget {
    // Not Claude: a Claude target wants `.claude/settings.json` in the tree.
    StageTarget::new(
        "codex",
        "",
        "",
        "",
        world.runner.clone(),
        world.pool.clone(),
    )
}

fn reviewer() -> ReviewerConfig {
    ReviewerConfig {
        id: "codex".to_string(),
        executor: "codex".to_string(),
        ..ReviewerConfig::default()
    }
}

fn clear_receipt() -> Receipt {
    receipt(&worktree("t1").path, GateStatus::Clear, Vec::new())
}

/// `Blocked` by the head lock alone, with a passing command.
fn lock_only_receipt() -> Receipt {
    let mut r = receipt(
        &worktree("t1").path,
        GateStatus::Blocked,
        vec![
            "head lock: 1 locked file(s) changed without a signed approval".to_string(),
            crate::gate::HEAD_LOCK_ONLY_NOTE.to_string(),
        ],
    );
    r.head_lock = HeadLockReport {
        status: LockStatus::Blocked,
        lock_fp: "fp".to_string(),
        locked_changes: Vec::new(),
        approval: None,
    };
    r.commands = vec![CommandResult {
        name: "test_scoped".to_string(),
        command: "true".to_string(),
        exit_code: Some(0),
        timed_out: false,
        duration_ms: 1,
        log_tail: String::new(),
    }];
    r
}

fn run_turn(world: &World, inputs: &ReviewInputs) -> PartialTurnResult {
    let deps = world.deps();
    let grant = Grant::new(None, None);
    run_reviewer_turn(
        &deps,
        &target(world),
        &reviewer(),
        &job(),
        &clear_receipt(),
        "",
        &grant,
        inputs,
    )
    .unwrap()
}

fn prompts(world: &World) -> Vec<String> {
    let state = world.runner.0.lock().unwrap();
    state.requests.iter().map(|r| r.prompt.clone()).collect()
}

fn short_circuited(world: &World) -> bool {
    world
        .db
        .events_for_task("t1", 100)
        .unwrap()
        .iter()
        .any(|e| {
            e.kind == EventKind::DualReviewTurn
                && e.payload_json.contains("\"short_circuited\":true")
        })
}

#[test]
fn the_current_inputs_are_what_the_factory_always_used() {
    let mut world = World::new();
    world.repo.cross_audit.score_gate = true;
    world.repo.cross_audit.min_average_score = 91;
    world.repo.cross_audit.min_criterion_score = 77;
    world.repo.cross_audit.max_diff_bytes = 12_345;
    world.add("t1");
    let inputs = ReviewInputs::current(&world.deps());
    assert!(inputs.policy.score_gate);
    assert_eq!(inputs.policy.min_average_score, 91);
    assert_eq!(inputs.policy.min_criterion_score, 77);
    assert_eq!(inputs.policy.max_diff_bytes, 12_345);
    assert_eq!(inputs.diff_base, None);
    assert_eq!(inputs.proof_ok, None);
}

#[test]
fn without_a_diff_base_the_task_diff_is_reviewed() {
    let world = World::new();
    world.add("t1");
    let inputs = ReviewInputs::current(&world.deps());
    run_turn(&world, &inputs);
    assert!(world.git.0.lock().unwrap().diff_from_calls.is_empty());
    let sent = prompts(&world);
    assert!(!sent.is_empty());
    assert!(sent[0].contains("change of t1"), "{}", sent[0]);
}

#[test]
fn a_diff_base_reviews_the_diff_from_that_base() {
    let world = World::new();
    world.add("t1");
    world.git.0.lock().unwrap().diffs.insert(
        worktree("t1").path,
        "diff --git a/src/pr.rs b/src/pr.rs\n--- a/src/pr.rs\n+++ b/src/pr.rs\n@@ -1 +1,2 @@\n \
         fn pr() {}\n+fn only_in_the_pr() {}\n"
            .to_string(),
    );
    let mut inputs = ReviewInputs::current(&world.deps());
    inputs.diff_base = Some(BASE.to_string());
    run_turn(&world, &inputs);
    assert_eq!(
        world.git.0.lock().unwrap().diff_from_calls,
        vec![BASE.to_string()]
    );
    assert!(prompts(&world)[0].contains("only_in_the_pr"));
}

#[test]
fn the_policy_comes_from_the_inputs_not_from_the_repo_config() {
    let world = World::new();
    world.add("t1");
    // Same repo config both times; only the inputs differ.
    let current = ReviewInputs::current(&world.deps());
    run_turn(&world, &current);
    assert!(!short_circuited(&world), "the default limit is generous");

    let mut small = ReviewInputs::current(&world.deps());
    small.policy.max_diff_bytes = 10;
    let before = prompts(&world).len();
    run_turn(&world, &small);
    assert!(short_circuited(&world), "a 10-byte limit refuses the diff");
    assert_eq!(prompts(&world).len(), before, "and no reviewer is asked");
}

#[test]
fn proof_ok_none_keeps_the_factory_scoring_and_some_overrides_it() {
    let world = World::new();
    world.add("t1");
    let deps = world.deps();
    let clear = clear_receipt();
    // Some(true) on a clear receipt: nothing hard-fails, the score is a real one.
    assert!(dual_mech_score_with(&deps, &job(), &clear, Some(true)) > 0);
    // Some(false): the proof did not pass, hard fail.
    assert_eq!(dual_mech_score_with(&deps, &job(), &clear, Some(false)), 0);
    // None asks `post_audit::proof_verified` as before: with no proof
    // required by the repo that is a pass, the same score as Some(true).
    assert_eq!(
        dual_mech_score_with(&deps, &job(), &clear, None),
        dual_mech_score_with(&deps, &job(), &clear, Some(true))
    );
}

#[test]
fn a_receipt_blocked_only_by_the_head_lock_scores_when_adopt_vouches_for_the_proof() {
    let world = World::new();
    world.add("t1");
    let deps = world.deps();
    let locked = lock_only_receipt();
    assert!(dual_mech_score_with(&deps, &job(), &locked, Some(true)) > 0);
    // The old path still scores a Blocked receipt 0.
    assert_eq!(dual_mech_score_with(&deps, &job(), &locked, None), 0);
}

#[test]
fn a_receipt_blocked_for_any_other_reason_still_scores_zero() {
    let world = World::new();
    world.add("t1");
    let deps = world.deps();
    let mut failing = lock_only_receipt();
    failing.commands[0].exit_code = Some(1);
    failing
        .reasons
        .push("command test_scoped failed (exit 1)".to_string());
    assert_eq!(dual_mech_score_with(&deps, &job(), &failing, Some(true)), 0);
    let mut empty = lock_only_receipt();
    empty.commands.clear();
    assert_eq!(dual_mech_score_with(&deps, &job(), &empty, Some(true)), 0);
    let mut skipped = lock_only_receipt();
    skipped
        .reasons
        .push("commands skipped: an earlier stage already blocks".to_string());
    assert_eq!(dual_mech_score_with(&deps, &job(), &skipped, Some(true)), 0);
}

#[test]
fn run_turn_arc_keeps_the_runner_alive() {
    // The runner is shared with the world, so its log is the world's too.
    let world = World::new();
    world.add("t1");
    let runner = Arc::clone(&world.runner);
    run_turn(&world, &ReviewInputs::current(&world.deps()));
    assert!(!runner.0.lock().unwrap().requests.is_empty());
}
