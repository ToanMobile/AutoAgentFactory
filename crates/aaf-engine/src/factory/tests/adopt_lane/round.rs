//! The lane's good paths: a PR with a Hard-locked file goes verify -> review
//! -> sign -> merge -> `Done`; a PR without locks needs no signature.

use aaf_domain::event::EventKind;
use aaf_domain::ports::KvStore;
use aaf_domain::task::TaskState;

use super::support::{Cfg, ID, Lab, Pr};
use crate::factory::tests::git_real::git;
use crate::gate::base::Base;
use crate::gate::head_lock;

fn payload(event: &aaf_domain::event::FactoryEvent) -> serde_json::Value {
    serde_json::from_str(&event.payload_json).unwrap()
}

#[test]
fn a_hard_locked_pr_is_reviewed_signed_merged_and_cleaned_up() {
    let lab = Lab::new(Pr::Locked);
    let done = lab.turn();
    assert_eq!(done.error, None);
    assert_eq!(lab.state(), TaskState::Done, "{:?}", lab.abort_code());
    assert_eq!(lab.gh.merges(), 1, "merged exactly once");
    assert_eq!(lab.reviewer_runs(), 2, "exactly two reviewer turns");
    assert!(lab.approvals_on_disk() > 0, "the lock was signed");
    assert!(lab.gh.comments().is_empty(), "no word on a PR that merged");

    // The merge result is what `aaf gc` and the startup sweep clean by.
    let merged = lab.events(EventKind::MergeResult);
    assert_eq!(merged.len(), 1);
    let value = payload(&merged[0]);
    assert_eq!(value["merged"], true);
    assert_eq!(value["head_sha"], lab.head.as_str());
    assert_eq!(value["kind"], "adopt");

    // `aaf/<id>` and its worktree are gone; the chat's own branch stays.
    assert!(!lab.ref_exists(&format!("refs/heads/aaf/{ID}")));
    assert!(!lab.adopt_wt().exists());
    assert!(
        lab.ref_exists("refs/heads/feat/pr"),
        "the chat's branch is kept"
    );

    // The phases were recorded, no abort, and nothing counted as a failure.
    let phases: Vec<String> = lab
        .events(EventKind::AdoptStep)
        .iter()
        .map(|e| payload(e)["phase"].as_str().unwrap().to_string())
        .collect();
    for phase in ["verify", "review", "merge"] {
        assert!(phases.iter().any(|p| p == phase), "{phase} in {phases:?}");
    }
    assert!(lab.events(EventKind::AdoptAborted).is_empty());
    assert_eq!(
        lab.world
            .db
            .get_meta(aaf_domain::ports::META_CONSECUTIVE_ROOT_FAILS)
            .unwrap(),
        None
    );
}

#[test]
fn a_pr_that_locks_nothing_merges_without_a_signature() {
    let lab = Lab::new(Pr::Lib);
    lab.turn();
    assert_eq!(lab.state(), TaskState::Done, "{:?}", lab.abort_code());
    assert_eq!(lab.gh.merges(), 1);
    assert_eq!(lab.reviewer_runs(), 2, "two reviewers even without a lock");
    assert_eq!(lab.approvals_on_disk(), 0, "nothing was signed");
}

#[test]
fn the_reviewers_run_twice_and_no_adjudication_even_when_dual_review_is_off() {
    let lab = Lab::with(
        Pr::Lib,
        Cfg {
            cross: "dual_review = false\ncross_adjudicate = true\n".to_string(),
            ..Cfg::default()
        },
    );
    lab.turn();
    assert_eq!(lab.state(), TaskState::Done, "{:?}", lab.abort_code());
    assert_eq!(lab.reviewer_runs(), 2);
    for runner in &lab.runners {
        let state = runner.fake.0.lock().unwrap();
        assert_eq!(state.requests.len(), 1, "one turn per reviewer");
        assert!(
            !state.requests[0].prompt.contains("adjudicat"),
            "no adjudication prompt"
        );
    }
}

#[test]
fn an_approval_already_on_disk_is_ignored_the_lane_reviews_and_signs_again() {
    let lab = Lab::new(Pr::Locked);
    // Somebody (a chat, an earlier run) already signed exactly this lock.
    let tip = git(&lab.work, &["rev-parse", "origin/main"]);
    let merge_base = git(&lab.work, &["merge-base", &tip, &lab.head]);
    let raw = git(
        &lab.work,
        &["show", &format!("{tip}:.autofactory/config.toml")],
    );
    let cfg = aaf_domain::builtin_defaults::parse_repo(&raw).unwrap();
    let wt = lab.adopt_wt();
    let report = head_lock::evaluate(&lab.data, &wt, &cfg, &merge_base);
    assert!(!report.lock_fp.is_empty());
    let base = Base {
        sha: Some(tip),
        diff_target: merge_base,
        head_sha: Some(lab.head.clone()),
    };
    head_lock::approve_expecting(
        &lab.data,
        &wt,
        &cfg,
        &base,
        ("someone-else", false),
        &*lab.deps().clock,
        &report.lock_fp,
    )
    .unwrap();

    lab.turn();
    assert_eq!(lab.state(), TaskState::Done, "{:?}", lab.abort_code());
    assert_eq!(lab.reviewer_runs(), 2, "the old approval saved no review");
    // The signature on disk is now the lane's, after its two reviewers.
    let key = aaf_fp::load_key(&lab.data).unwrap();
    let approval = aaf_fp::read_approval(&lab.data, &key, &lab.work, &report.lock_fp)
        .unwrap()
        .expect("an approval for the lock");
    assert_eq!(approval.reviewer, "adopt-dual:codex+opencode");
    assert!(!approval.same_account);
}

#[test]
fn a_base_that_edits_a_locked_file_after_a_pr_that_does_not_lock_anything_still_merges() {
    // The PR's own diff starts at the merge-base: what the base added since is
    // not the PR's change, so it is neither a lock nor a deletion.
    let lab = Lab::new(Pr::Lib);
    lab.advance_base(|root| {
        std::fs::write(
            root.join("Cargo.toml"),
            super::support::padded_cargo("# base touched\n", ""),
        )
        .unwrap();
    });
    lab.turn();
    assert_eq!(lab.state(), TaskState::Done, "{:?}", lab.abort_code());
    assert_eq!(lab.approvals_on_disk(), 0, "nothing was signed");
    assert_eq!(lab.gh.merges(), 1);
}
