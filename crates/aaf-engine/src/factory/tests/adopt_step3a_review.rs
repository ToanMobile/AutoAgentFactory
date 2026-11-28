//! `aaf board adopt` step 3a, case 6: one `run_reviewer_turn` on a `GateJob`
//! built from an adopt worktree (`aaf/<id>` at the pinned head, a linked
//! worktree of a real repository), with a fake reviewer that answers PASS.
//! Asked: does the reviewer get `merge-base...pinned` and nothing from the
//! base's own progress, and is `mech` not 0 for a receipt that only the head
//! lock blocks.

use std::fs;
use std::sync::Arc;

use aaf_domain::event::EventKind;
use aaf_domain::gate::{CommandResult, GateStatus, HeadLockReport, LockStatus, Receipt};
use aaf_domain::pipeline::ReviewerConfig;
use aaf_domain::ports::{EventSink, TaskStore};
use aaf_domain::task::{Priority, Task, TaskState};

use super::fakes::{REPO, receipt, spec};
use super::git_real::{Fixture, git};
use super::world::World;
use crate::cross_audit::dual_verdict::build_dual_checklist;
use crate::factory::dual_audit::{
    PartialTurnResult, ReviewInputs, ReviewPolicy, run_reviewer_turn,
};
use crate::factory::stage_router::StageTarget;
use crate::factory::worker::GateJob;
use crate::factory::{Grant, RealGit, resolve_base};
use crate::git::TaskWorktree;

/// A repository whose `main` moved on after the PR branched, and an adopt
/// worktree of the PR head. Returns (fixture, job, merge-base, pinned head).
fn adopt_fixture() -> (Fixture, GateJob, String, String) {
    let fx = Fixture::new();
    let merge_base = git(&fx.repo, &["rev-parse", "HEAD"]);
    git(&fx.repo, &["checkout", "-q", "-b", "feat/pr"]);
    fs::write(fx.repo.join("pr_only.txt"), "only in the pr\n").unwrap();
    git(&fx.repo, &["add", "-A"]);
    git(&fx.repo, &["commit", "-q", "-m", "pr"]);
    let pinned = git(&fx.repo, &["rev-parse", "HEAD"]);
    git(&fx.repo, &["checkout", "-q", "main"]);
    fs::write(fx.repo.join("base_only.txt"), "base moved on\n").unwrap();
    git(&fx.repo, &["add", "-A"]);
    git(&fx.repo, &["commit", "-q", "-m", "base moves"]);
    git(&fx.repo, &["update-ref", "refs/heads/aaf/t1", &pinned]);
    let path = fx.tmp.path().join("adopt-wt");
    git(
        &fx.repo,
        &["worktree", "add", "-q", path.to_str().unwrap(), "aaf/t1"],
    );
    let job = GateJob {
        task_id: "t1".to_string(),
        worktree: TaskWorktree {
            path,
            branch: "aaf/t1".to_string(),
        },
        audited: false,
        cached_receipt: None,
    };
    (fx, job, merge_base, pinned)
}

/// What the adopt dry run leaves: `Blocked` by the head lock alone, one
/// passing command.
fn lock_only_receipt(job: &GateJob) -> Receipt {
    let mut r = receipt(
        &job.worktree.path,
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

/// A reviewer that finds nothing: every checklist question answered "no".
fn script_pass(world: &World) {
    let answers: Vec<_> = build_dual_checklist(&spec(), world.deps().settings.mobile, false)
        .iter()
        .map(|i| serde_json::json!({"id": i.id, "answer": "no", "evidence": ""}))
        .collect();
    world.runner.0.lock().unwrap().audit_text =
        Some(serde_json::json!({"answers": answers, "issues": []}).to_string());
}

/// The task row the lane would hold: a root in `Adopting`.
fn add_adopting(world: &World) {
    let mut task = Task::new_root("t1", REPO, "adopt t1", spec(), Priority::P2, 1);
    task.state = TaskState::Adopting;
    world.db.insert_task(&task).unwrap();
}

fn prompts(world: &World) -> Vec<String> {
    let state = world.runner.0.lock().unwrap();
    state.requests.iter().map(|r| r.prompt.clone()).collect()
}

fn turn(world: &World, fx: &Fixture, job: &GateJob, inputs: &ReviewInputs) -> PartialTurnResult {
    let mut deps = world.deps();
    deps.git = Arc::new(RealGit::new(
        fx.repo.clone(),
        fx.tmp.path().join("worktrees"),
        fx.tmp.path().join("data/locks"),
        resolve_base(&fx.repo, "auto").unwrap(),
    ));
    let target = StageTarget::new(
        "codex",
        "",
        "",
        "",
        world.runner.clone(),
        world.pool.clone(),
    );
    let reviewer = ReviewerConfig {
        id: "codex".to_string(),
        executor: "codex".to_string(),
        ..ReviewerConfig::default()
    };
    run_reviewer_turn(
        &deps,
        &target,
        &reviewer,
        job,
        &lock_only_receipt(job),
        "",
        &Grant::new(None, None),
        inputs,
    )
    .unwrap()
}

#[test]
fn a_reviewer_turn_on_an_adopt_worktree_sees_the_pr_diff_and_a_real_mech_score() {
    let (fx, job, merge_base, pinned) = adopt_fixture();
    let world = World::new();
    add_adopting(&world);
    script_pass(&world);
    let inputs = ReviewInputs {
        policy: ReviewPolicy::from_cross_audit(&world.repo.cross_audit),
        diff_base: Some(merge_base.clone()),
        proof_ok: Some(true),
    };
    assert_ne!(merge_base, pinned);

    let PartialTurnResult::Completed(rec) = turn(&world, &fx, &job, &inputs) else {
        panic!("deferred");
    };
    let sent = prompts(&world);
    eprintln!(
        "CASE6 verdict={} mech={} prompts={}",
        rec.verdict,
        rec.mechanical_score,
        sent.len()
    );
    assert!(!sent.is_empty(), "the reviewer was asked");
    assert!(sent[0].contains("pr_only.txt"), "the PR's own change");
    assert!(
        !sent[0].contains("base_only.txt"),
        "nothing the base added since the merge-base"
    );
    assert!(
        rec.mechanical_score > 0,
        "mech is not 0 for a lock-only block"
    );
    assert_eq!(rec.verdict, "PASS", "{rec:?}");
    assert_eq!((rec.p0, rec.p1), (0, 0));
    let events = world.db.events_for_task("t1", 100).unwrap();
    assert!(events.iter().any(|e| e.kind == EventKind::DualReviewTurn));
}

#[test]
fn without_proof_ok_the_same_receipt_scores_zero() {
    // The control: the old path (`proof_ok: None`) does not know adopt
    // vouches for the proof and scores a Blocked receipt 0.
    let (fx, job, merge_base, _) = adopt_fixture();
    let world = World::new();
    add_adopting(&world);
    script_pass(&world);
    let inputs = ReviewInputs {
        policy: ReviewPolicy::from_cross_audit(&world.repo.cross_audit),
        diff_base: Some(merge_base),
        proof_ok: None,
    };
    let PartialTurnResult::Completed(rec) = turn(&world, &fx, &job, &inputs) else {
        panic!("deferred");
    };
    assert_eq!(rec.mechanical_score, 0);
}
