#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `aaf board adopt` step 2, merge side: `strict_merged_head`,
//! `merge_pr_with_check` and `keep_non_aaf_local`. Each has a test with it
//! off (the old behaviour) and one with it on.

mod adopt_step2_support;

use std::cell::Cell;

use aaf_engine::pr_ops::PrMergeOutcome;
use adopt_step2_support::{FakeGh, Fx, code_of, ref_exists};

// strict_merged_head ------------------------------------------------------------

#[test]
fn without_strict_merged_head_a_merged_pr_at_another_head_is_already_merged() {
    let fx = Fx::counting();
    let head = fx.edit_lib("feat/a");
    let other = "b".repeat(40);
    let gh = FakeGh::new(4, "feat/a", &head).merged_at(&other);
    let out = fx.run(&fx.req(4, &head, true), &gh);
    assert!(
        matches!(out, PrMergeOutcome::AlreadyMerged { pr_number: 4, .. }),
        "{out:?}"
    );
}

#[test]
fn strict_merged_head_blocks_a_merged_pr_at_another_head() {
    let fx = Fx::counting();
    let head = fx.edit_lib("feat/a");
    let other = "b".repeat(40);
    let gh = FakeGh::new(4, "feat/a", &head).merged_at(&other);
    let mut req = fx.req(4, &head, true);
    req.strict_merged_head = true;
    assert_eq!(code_of(&fx.run(&req, &gh)), "merged_unpinned");
    // Also when it is a real (not dry) run.
    req.dry_run = false;
    assert_eq!(code_of(&fx.run(&req, &gh)), "merged_unpinned");
}

#[test]
fn strict_merged_head_keeps_already_merged_at_the_pinned_head() {
    let fx = Fx::counting();
    let head = fx.edit_lib("feat/a");
    let gh = FakeGh::new(4, "feat/a", &head).merged_at(&head);
    let mut req = fx.req(4, &head, true);
    req.strict_merged_head = true;
    let out = fx.run(&req, &gh);
    assert!(
        matches!(out, PrMergeOutcome::AlreadyMerged { pr_number: 4, .. }),
        "{out:?}"
    );
}

// merge_pr_with_check ---------------------------------------------------------------

#[test]
fn a_check_that_says_no_stops_the_merge_before_gh_is_called() {
    let fx = Fx::counting();
    let head = fx.edit_lib("feat/a");
    let gh = FakeGh::new(5, "feat/a", &head);
    let asked = Cell::new(0);
    let no = || {
        asked.set(asked.get() + 1);
        false
    };
    let out = fx.run_checked(&fx.req(5, &head, false), &gh, Some(&no));
    assert_eq!(code_of(&out), "cancelled_before_merge");
    assert_eq!(asked.get(), 1);
    assert_eq!(gh.merges(), 0, "gh pr merge must not run");
}

#[test]
fn a_check_that_says_yes_lets_the_merge_through() {
    let fx = Fx::counting();
    let head = fx.edit_lib("feat/a");
    let gh = FakeGh::new(5, "feat/a", &head);
    let yes = || true;
    let out = fx.run_checked(&fx.req(5, &head, false), &gh, Some(&yes));
    assert!(
        matches!(out, PrMergeOutcome::Merged { pr_number: 5, .. }),
        "{out:?}"
    );
    assert_eq!(gh.merges(), 1);
}

#[test]
fn no_check_is_the_old_merge() {
    let fx = Fx::counting();
    let head = fx.edit_lib("feat/a");
    let gh = FakeGh::new(5, "feat/a", &head);
    let out = fx.run_checked(&fx.req(5, &head, false), &gh, None);
    assert!(
        matches!(out, PrMergeOutcome::Merged { pr_number: 5, .. }),
        "{out:?}"
    );
    assert_eq!(gh.merges(), 1);
}

#[test]
fn the_check_is_never_asked_on_a_dry_run() {
    let fx = Fx::counting();
    let head = fx.edit_lib("feat/a");
    let gh = FakeGh::new(5, "feat/a", &head);
    let never = || -> bool { panic!("a dry run merges nothing") };
    let out = fx.run_checked(&fx.req(5, &head, true), &gh, Some(&never));
    assert!(matches!(out, PrMergeOutcome::DryRunClear { .. }), "{out:?}");
}

// keep_non_aaf_local ----------------------------------------------------------------

#[test]
fn without_keep_non_aaf_local_a_merged_branch_is_deleted_locally() {
    let fx = Fx::counting();
    let head = fx.edit_lib("feat/chat");
    let gh = FakeGh::new(6, "feat/chat", &head);
    let out = fx.run(&fx.req(6, &head, false), &gh);
    assert!(matches!(out, PrMergeOutcome::Merged { .. }), "{out:?}");
    assert!(!ref_exists(fx.work.path(), "refs/heads/feat/chat"));
    assert!(!ref_exists(&fx.bare(), "refs/heads/feat/chat"));
}

#[test]
fn keep_non_aaf_local_keeps_the_local_branch_but_still_deletes_the_remote_one() {
    let fx = Fx::counting();
    let head = fx.edit_lib("feat/chat");
    let gh = FakeGh::new(6, "feat/chat", &head);
    let mut req = fx.req(6, &head, false);
    req.keep_non_aaf_local = true;
    let out = fx.run(&req, &gh);
    assert!(matches!(out, PrMergeOutcome::Merged { .. }), "{out:?}");
    assert!(
        ref_exists(fx.work.path(), "refs/heads/feat/chat"),
        "local branch kept"
    );
    assert!(
        !ref_exists(&fx.bare(), "refs/heads/feat/chat"),
        "remote branch deleted"
    );
}

#[test]
fn keep_non_aaf_local_still_cleans_an_aaf_branch() {
    let fx = Fx::counting();
    let head = fx.edit_lib("aaf/t-keep");
    let gh = FakeGh::new(7, "aaf/t-keep", &head);
    let mut req = fx.req(7, &head, false);
    req.keep_non_aaf_local = true;
    let out = fx.run(&req, &gh);
    assert!(matches!(out, PrMergeOutcome::Merged { .. }), "{out:?}");
    assert!(!ref_exists(fx.work.path(), "refs/heads/aaf/t-keep"));
}
