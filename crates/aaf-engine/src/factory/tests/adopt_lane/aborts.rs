//! Every way the lane cancels an adopted PR. Each test names the code, and
//! checks the same things: the task is `Cancelled`, `AdoptAborted` carries the
//! code, the PR got a comment that starts `🤖 aaf-bot:` and names the code,
//! nothing merged, nothing counted as a root failure.

use aaf_domain::event::EventKind;
use aaf_domain::ports::{KvStore, META_CONSECUTIVE_ROOT_FAILS, TaskStore};
use aaf_domain::task::TaskState;

use super::support::{Cfg, ID, Lab, Pr, padded_cargo};
use crate::factory::tests::git_real::git;

/// The shared claims of every cancel.
fn assert_cancelled(lab: &Lab, code: &str) {
    assert_eq!(lab.state(), TaskState::Cancelled, "task state");
    assert_eq!(lab.abort_code().as_deref(), Some(code), "abort code");
    assert_eq!(lab.gh.merges(), 0, "nothing merged");
    let comments = lab.gh.comments();
    assert_eq!(comments.len(), 1, "one comment on the PR: {comments:?}");
    assert!(comments[0].starts_with("🤖 aaf-bot:"), "{}", comments[0]);
    assert!(comments[0].contains(code), "{}", comments[0]);
    assert!(lab.task().retry_after.is_none(), "not scheduled again");
    assert_eq!(
        lab.world.db.get_meta(META_CONSECUTIVE_ROOT_FAILS).unwrap(),
        None,
        "a cancelled adoption is not a root failure"
    );
    assert_eq!(lab.events(EventKind::AdoptAborted).len(), 1);
}

#[test]
fn needs_human_head_lock_when_the_pr_changes_the_ci_that_judges_it() {
    let lab = Lab::new(Pr::Workflow);
    lab.turn();
    assert_cancelled(&lab, "needs_human_head_lock");
    assert_eq!(lab.reviewer_runs(), 0, "no review is spent on it");
    assert_eq!(lab.commands_ran(), 0, "no gate command ran for it");
}

#[test]
fn lock_changed_when_the_pr_and_the_base_both_change_a_locked_file() {
    let lab = Lab::new(Pr::Locked);
    lab.advance_base(|root| {
        std::fs::write(
            root.join("Cargo.toml"),
            padded_cargo("# base touched\n", ""),
        )
        .unwrap();
    });
    lab.turn();
    assert_cancelled(&lab, "lock_changed");
    assert_eq!(lab.reviewer_runs(), 0, "decided before any review");
    assert_eq!(lab.approvals_on_disk(), 0, "nothing signed");
}

#[test]
fn lock_changed_when_the_base_moves_the_locked_file_after_the_signature() {
    let lab = Lab::new(Pr::Locked);
    let work = lab.work.clone();
    // While the second reviewer reads, the base edits the locked file.
    *lab.runners[1].before.lock().unwrap() = Some(Box::new(move || {
        std::fs::write(
            work.join("Cargo.toml"),
            padded_cargo("# base touched\n", ""),
        )
        .unwrap();
        git(&work, &["checkout", "main"]);
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-m", "base moves"]);
        git(&work, &["push", "origin", "main"]);
    }));
    lab.turn();
    assert_cancelled(&lab, "lock_changed");
    assert_eq!(lab.reviewer_runs(), 2, "the review had happened");
}

#[test]
fn needs_human_reviewers_with_a_single_provider() {
    let lab = Lab::with(
        Pr::Lib,
        Cfg {
            auditor: "codex",
            ..Cfg::default()
        },
    );
    lab.turn();
    assert_cancelled(&lab, "needs_human_reviewers");
    assert_eq!(lab.reviewer_runs(), 0);
}

#[test]
fn needs_human_reviewers_when_a_reviewer_of_the_base_config_is_not_wired() {
    let lab = Lab::with(
        Pr::Lib,
        Cfg {
            cross: "\n[[cross_audit.reviewers]]\nid = \"unwired\"\nexecutor = \"opencode\"\n"
                .to_string(),
            ..Cfg::default()
        },
    );
    lab.turn();
    assert_cancelled(&lab, "needs_human_reviewers");
    assert_eq!(lab.reviewer_runs(), 0);
}

#[test]
fn review_failed_when_one_reviewer_passes_and_one_fails_and_nothing_is_signed() {
    let lab = Lab::new(Pr::Locked);
    lab.script_fail(&lab.runners[1].fake);
    lab.turn();
    assert_cancelled(&lab, "review_failed");
    assert_eq!(lab.reviewer_runs(), 2, "both reviewers were asked");
    assert_eq!(
        lab.approvals_on_disk(),
        0,
        "no signature without two passes"
    );
    let comment = &lab.gh.comments()[0];
    assert!(comment.contains("the answer is wrong"), "{comment}");
}

#[test]
fn gate_blocked_when_the_commands_fail() {
    let lab = Lab::with(
        Pr::Lib,
        Cfg {
            test: "false".to_string(),
            ..Cfg::default()
        },
    );
    lab.turn();
    assert_cancelled(&lab, "gate_blocked");
    assert_eq!(
        lab.reviewer_runs(),
        0,
        "no review of a PR that fails its tests"
    );
}

#[test]
fn gate_blocked_not_lock_changed_when_a_locked_pr_fails_its_commands_in_the_dry_run() {
    let lab = Lab::with(
        Pr::Locked,
        Cfg {
            test: "false".to_string(),
            ..Cfg::default()
        },
    );
    lab.turn();
    assert_cancelled(&lab, "gate_blocked");
}

#[test]
fn head_changed_when_the_branch_ref_moved_before_verify() {
    let lab = Lab::new(Pr::Lib);
    let base = git(&lab.work, &["rev-parse", "origin/main"]);
    git(
        &lab.work,
        &["update-ref", &format!("refs/heads/aaf/{ID}"), &base],
    );
    lab.turn();
    assert_cancelled(&lab, "head_changed");
}

#[test]
fn head_changed_when_the_adopt_worktree_is_dirty() {
    let lab = Lab::new(Pr::Lib);
    std::fs::write(lab.adopt_wt().join("scratch.txt"), "x\n").unwrap();
    lab.turn();
    assert_cancelled(&lab, "head_changed");
}

#[test]
fn head_changed_when_the_adopt_worktree_gets_a_new_commit() {
    let lab = Lab::new(Pr::Lib);
    let wt = lab.adopt_wt();
    std::fs::write(wt.join("more.txt"), "x\n").unwrap();
    git(&wt, &["add", "-A"]);
    git(&wt, &["commit", "-m", "sneaked in"]);
    lab.turn();
    assert_cancelled(&lab, "head_changed");
}

#[test]
fn head_changed_when_github_reports_another_head() {
    let lab = Lab::new(Pr::Lib);
    lab.gh.pr.lock().unwrap().head_ref_oid = "1".repeat(40);
    lab.turn();
    assert_cancelled(&lab, "head_changed");
}

#[test]
fn head_changed_between_verify_and_review() {
    let lab = Lab::new(Pr::Lib);
    let wt = lab.adopt_wt();
    // The third `gh pr view` is the dry run's, after the gate: the worktree is
    // dirtied behind the lane's back right before the review begins.
    lab.gh.on_view.lock().unwrap().push((
        3,
        Box::new(move || std::fs::write(wt.join("late.txt"), "x\n").unwrap()),
    ));
    lab.turn();
    assert_cancelled(&lab, "head_changed");
    assert_eq!(lab.reviewer_runs(), 0, "no review on a moved pin");
}

#[test]
fn head_changed_between_review_and_merge() {
    let lab = Lab::new(Pr::Lib);
    let wt = lab.adopt_wt();
    *lab.runners[1].before.lock().unwrap() = Some(Box::new(move || {
        std::fs::write(wt.join("late.txt"), "x\n").unwrap();
    }));
    lab.turn();
    assert_cancelled(&lab, "head_changed");
    assert_eq!(lab.reviewer_runs(), 2);
}

#[test]
fn head_changed_when_the_pr_event_is_missing_so_no_head_is_pinned() {
    let lab = Lab::new(Pr::Lib);
    // A task with no `PrAdopted` event: the lane guesses nothing.
    let mut task = lab.task();
    task.id = "orphan".to_string();
    task.root_id = "orphan".to_string();
    lab.world.db.insert_task(&task).unwrap();
    let done = crate::factory::adopt_lane::run_job(&lab.deps(), &lab.router, "orphan");
    assert_eq!(done.error, None);
    assert_eq!(lab.world.state("orphan"), TaskState::Cancelled);
    assert_eq!(lab.gh.merges(), 0);
}

#[test]
fn head_changed_when_the_pinned_head_in_the_event_is_not_a_commit() {
    let lab = Lab::new(Pr::Lib);
    let mut task = lab.task();
    task.id = "bad-pin".to_string();
    task.root_id = "bad-pin".to_string();
    lab.world.db.insert_task(&task).unwrap();
    let payload = serde_json::json!({
        "pr_number": 7, "url": "u", "head_sha": "not-a-sha", "head_ref": "feat/pr",
        "base_ref": "main", "author_login": "owner",
    });
    let event = aaf_domain::event::FactoryEvent::new(EventKind::PrAdopted, payload.to_string(), 1)
        .for_task("bad-pin");
    aaf_domain::ports::EventSink::emit(&lab.world.db, &event).unwrap();
    crate::factory::adopt_lane::run_job(&lab.deps(), &lab.router, "bad-pin");
    assert_eq!(lab.world.state("bad-pin"), TaskState::Cancelled);
    assert_eq!(lab.gh.merges(), 0);
    assert_eq!(lab.gh.views.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn merged_unpinned_when_the_pr_was_merged_at_another_head() {
    let lab = Lab::new(Pr::Lib);
    {
        let mut pr = lab.gh.pr.lock().unwrap();
        pr.state = "MERGED".to_string();
        pr.head_ref_oid = "2".repeat(40);
    }
    lab.turn();
    assert_cancelled(&lab, "merged_unpinned");
}

#[test]
fn a_pr_already_merged_at_the_pinned_head_is_done_without_merging_again() {
    // A turn that merged and then died: the next one finishes the bookkeeping.
    let lab = Lab::new(Pr::Lib);
    lab.gh.set_state("MERGED");
    lab.turn();
    assert_eq!(lab.state(), TaskState::Done);
    assert_eq!(lab.gh.merges(), 0);
    assert!(lab.gh.comments().is_empty());
}

#[test]
fn pr_not_open_when_the_pr_was_closed() {
    let lab = Lab::new(Pr::Lib);
    lab.gh.set_state("CLOSED");
    lab.turn();
    assert_cancelled(&lab, "pr_not_open");
}

#[test]
fn pr_is_draft_when_the_pr_went_back_to_draft() {
    let lab = Lab::new(Pr::Lib);
    lab.gh.pr.lock().unwrap().is_draft = true;
    lab.turn();
    assert_cancelled(&lab, "pr_is_draft");
}

#[test]
fn ci_failed_when_a_check_failed() {
    let lab = Lab::new(Pr::Lib);
    lab.gh.set_checks("fail");
    lab.turn();
    assert_cancelled(&lab, "ci_failed");
}

#[test]
fn merge_conflict_when_the_base_changed_the_same_line() {
    let lab = Lab::new(Pr::Lib);
    lab.advance_base(|root| {
        std::fs::write(root.join("src/lib.rs"), "pub fn v() -> i32 { 3 }\n").unwrap();
    });
    lab.turn();
    assert_cancelled(&lab, "merge_conflict");
}

#[test]
fn cancelling_by_hand_during_the_review_stops_the_merge_and_says_nothing() {
    let lab = Lab::new(Pr::Lib);
    let (db, now) = (lab.world.db.clone(), 5_000);
    *lab.runners[1].before.lock().unwrap() = Some(Box::new(move || {
        assert!(
            db.compare_and_set_state(ID, TaskState::Adopting, TaskState::Cancelled, None, now)
                .unwrap()
        );
    }));
    let done = lab.turn();
    assert_eq!(done.error, None);
    assert_eq!(
        lab.state(),
        TaskState::Cancelled,
        "stays as the person left it"
    );
    assert_eq!(lab.gh.merges(), 0, "the pre-merge check refused");
    assert!(lab.gh.comments().is_empty(), "the lane adds nothing");
    assert!(lab.events(EventKind::AdoptAborted).is_empty());
}
