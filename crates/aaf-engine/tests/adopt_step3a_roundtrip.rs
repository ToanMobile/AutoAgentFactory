#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `aaf board adopt` step 3a spike: the sign -> merge round trip on a
//! Hard-locked file, with real git and a fake `gh`. It answers whether the
//! `lock_fp` computed on the adopt worktree (diff from the merge-base) is the
//! one the dry run reports for the merged scratch tree, and whether signing
//! it with `approve_expecting(Base { tip, merge_base, pinned })` lets the
//! non-dry `merge_pr_with_check` through. The reviewer turn (case 6) needs
//! crate-private items and lives in `src/factory/tests/adopt_step3a_review.rs`.

mod adopt_step2_support;

use std::fs;
use std::path::{Path, PathBuf};

use aaf_domain::config::RepoConfig;
use aaf_domain::gate::{Approval, LockStatus, LockTier};
use aaf_engine::gate::base::Base;
use aaf_engine::gate::head_lock;
use aaf_engine::pr_ops::{PrMergeOutcome, PrMergeRequest};
use adopt_step2_support::{FakeGh, FixedClock, Fx, code_of, git};
use tempfile::TempDir;

const REVIEWER: &str = "adopt-dual:codex+claude";

/// `Cargo.toml` is a Hard lock in the rust profile. It is padded so a PR edit
/// at the bottom and a base edit at the top merge without a conflict.
fn padded_cargo(top: &str, bottom: &str) -> String {
    let pad: String = (0..20).map(|i| format!("# pad {i}\n")).collect();
    format!("{top}[package]\nname = \"adopt\"\nversion = \"0.1.0\"\n{pad}{bottom}")
}

struct Adopt {
    fx: Fx,
    gh: FakeGh,
    head: String,
    /// The adopt worktree (`aaf/<id>` at the pinned head).
    wt: PathBuf,
    _wt_dir: TempDir,
}

fn setup_with(fx: Fx) -> Adopt {
    fs::write(fx.work.path().join("Cargo.toml"), padded_cargo("", "")).unwrap();
    git(fx.work.path(), &["add", "-A"]);
    git(fx.work.path(), &["commit", "-m", "pad cargo"]);
    git(fx.work.path(), &["push", "origin", "main"]);
    let head = fx.branch("feat/lock", |root| {
        fs::write(root.join("Cargo.toml"), padded_cargo("", "# pr line\n")).unwrap();
    });
    let gh = FakeGh::new(7, "feat/lock", &head);
    let wt_dir = TempDir::new().unwrap();
    let wt = wt_dir.path().join("adopt-t1");
    git(fx.work.path(), &["update-ref", "refs/heads/aaf/t1", &head]);
    git(
        fx.work.path(),
        &["worktree", "add", wt.to_str().unwrap(), "aaf/t1"],
    );
    Adopt {
        fx,
        gh,
        head,
        wt,
        _wt_dir: wt_dir,
    }
}

fn setup() -> Adopt {
    setup_with(Fx::counting())
}

impl Adopt {
    fn root(&self) -> &Path {
        self.fx.work.path()
    }

    fn data(&self) -> &Path {
        self.fx.data.path()
    }

    fn tip(&self) -> String {
        git(self.root(), &["rev-parse", "origin/main"])
    }

    fn merge_base(&self, tip: &str) -> String {
        git(self.root(), &["merge-base", tip, &self.head])
    }

    /// Base moves on with `edit` (a commit pushed to `origin/main`).
    fn advance_base(&self, edit: impl FnOnce(&Path)) {
        git(self.root(), &["checkout", "main"]);
        edit(self.root());
        git(self.root(), &["add", "-A"]);
        git(self.root(), &["commit", "-m", "base moves"]);
        git(self.root(), &["push", "origin", "main"]);
    }

    /// The adopt dry run: both flags on.
    fn adopt_dry_run(&self) -> PrMergeOutcome {
        let mut req = self.fx.req(7, &self.head, true);
        req.no_receipt_reuse = true;
        req.run_commands_despite_head_lock = true;
        self.fx.run(&req, &self.gh)
    }

    /// The trusted base config the way `load_trusted_base_config` reads it
    /// (that function is private to `pr_ops`; without Tier-1 overrides the
    /// two agree).
    fn trusted_cfg(&self, tip: &str) -> RepoConfig {
        let raw = git(
            self.root(),
            &["show", &format!("{tip}:.autofactory/config.toml")],
        );
        aaf_domain::builtin_defaults::parse_repo(&raw).unwrap()
    }

    fn lock_fp_on_adopt_wt(&self, tip: &str) -> (LockStatus, String, usize) {
        let rep = head_lock::evaluate(
            self.data(),
            &self.wt,
            &self.trusted_cfg(tip),
            &self.merge_base(tip),
        );
        (rep.status, rep.lock_fp, rep.locked_changes.len())
    }

    fn sign(&self, tip: &str, lock_fp: &str) -> Result<Approval, aaf_engine::gate::GateError> {
        let base = Base {
            sha: Some(tip.to_string()),
            diff_target: self.merge_base(tip),
            head_sha: Some(self.head.clone()),
        };
        head_lock::approve_expecting(
            self.data(),
            &self.wt,
            &self.trusted_cfg(tip),
            &base,
            (REVIEWER, false),
            &FixedClock(1_000),
            lock_fp,
        )
    }

    /// The adopt merge: dry_run = false with the three adopt flags.
    fn adopt_merge_req(&self) -> PrMergeRequest {
        let mut req = self.fx.req(7, &self.head, false);
        req.no_receipt_reuse = true;
        req.strict_merged_head = true;
        req.keep_non_aaf_local = true;
        req
    }

    fn approval_mirrored(&self, lock_fp: &str) -> bool {
        let key = aaf_fp::load_key(self.data()).unwrap();
        let mut roots = vec![self.root().to_path_buf()];
        roots.extend(fs::canonicalize(self.root()).ok());
        roots.iter().any(|r| {
            matches!(aaf_fp::read_approval(self.data(), &key, r, lock_fp), Ok(Some(a)) if a.passes_independent(lock_fp))
        })
    }
}

fn pending_fp(out: &PrMergeOutcome) -> (String, String) {
    match out {
        PrMergeOutcome::DryRunPendingHeadLock {
            lock_fp, base_sha, ..
        } => (lock_fp.clone(), base_sha.clone()),
        other => panic!("expected DryRunPendingHeadLock, got {other:?}"),
    }
}

// Case 1 ---------------------------------------------------------------------

#[test]
fn case1_the_round_trip_on_a_hard_locked_file_merges() {
    let a = setup();
    let out = a.adopt_dry_run();
    if let PrMergeOutcome::DryRunPendingHeadLock { receipt, .. } = &out {
        // The shape `pending_head_lock_only` accepts (and case 6 fakes).
        assert!(receipt.reasons.iter().any(|r| r.starts_with("head lock:")));
        assert!(!receipt.commands.is_empty() && receipt.commands.iter().all(|c| c.passed()));
    }
    let (fp_dry, tip) = pending_fp(&out);
    assert_eq!(a.fx.ran(), 1, "the dry run really ran the commands");
    assert_eq!(tip, a.tip());

    let (status, fp_adopt, n) = a.lock_fp_on_adopt_wt(&tip);
    eprintln!("CASE1 lock_fp dry={fp_dry} adopt={fp_adopt} status={status:?} locked={n}");
    assert_eq!(status, LockStatus::Blocked);
    assert_eq!(
        fp_adopt, fp_dry,
        "lock_fp on the adopt worktree == the dry run's"
    );

    let approval = a.sign(&tip, &fp_adopt).expect("signed");
    assert!(!approval.same_account);
    assert!(
        a.approval_mirrored(&fp_adopt),
        "mirror under the common root"
    );

    let out =
        a.fx.run_checked(&a.adopt_merge_req(), &a.gh, Some(&|| true));
    assert!(matches!(out, PrMergeOutcome::Merged { .. }), "{out:?}");
    assert_eq!(a.gh.merges(), 1);
    assert_eq!(a.fx.ran(), 2, "the merge gated again, with the approval");
}

#[test]
fn case1_signing_with_the_dry_runs_fp_is_the_same_signature() {
    // Not an alternative: the two fingerprints are equal, so signing with the
    // dry run's value is the same call. Kept as an explicit control.
    let a = setup();
    let (fp_dry, tip) = pending_fp(&a.adopt_dry_run());
    a.sign(&tip, &fp_dry)
        .expect("signs with the dry run's lock_fp");
    let out =
        a.fx.run_checked(&a.adopt_merge_req(), &a.gh, Some(&|| true));
    assert!(matches!(out, PrMergeOutcome::Merged { .. }), "{out:?}");
}

// Case 1 when the base already changed the locked file ------------------------

#[test]
fn case1c_a_base_that_changed_the_locked_file_before_the_dry_run_splits_the_two_fps() {
    // Base edits Cargo.toml (top) after the PR branched; the PR edits it (bottom).
    // The merged tree holds both edits, the adopt worktree only the PR's.
    let a = setup();
    a.advance_base(|root| {
        fs::write(
            root.join("Cargo.toml"),
            padded_cargo("# base touched\n", ""),
        )
        .unwrap();
    });
    let (fp_dry, tip) = pending_fp(&a.adopt_dry_run());
    let (status, fp_adopt, _) = a.lock_fp_on_adopt_wt(&tip);
    assert_eq!(status, LockStatus::Blocked);
    eprintln!("CASE1c dry={fp_dry} adopt={fp_adopt}");
    assert_ne!(
        fp_adopt, fp_dry,
        "the merged tree's lock_fp is not the PR's"
    );
    // Signing the dry run's fp from the adopt worktree is refused ...
    let err = a.sign(&tip, &fp_dry).expect_err("refused");
    eprintln!("CASE1c sign(fp_dry) -> {err:?}");
    assert!(matches!(err, aaf_engine::gate::GateError::LockFpChanged));
    // ... and the adopt worktree's fp, once signed, does not cover the merged tree.
    a.sign(&tip, &fp_adopt)
        .expect("signed on the adopt worktree");
    let out =
        a.fx.run_checked(&a.adopt_merge_req(), &a.gh, Some(&|| true));
    eprintln!("CASE1c merge -> {}", code_of(&out));
    assert_eq!(code_of(&out), "cross_approval_required");
    assert_eq!(a.gh.merges(), 0);
}

// Case 2 ---------------------------------------------------------------------

#[test]
fn case2_base_moving_on_another_file_still_merges() {
    let a = setup();
    let (fp_dry, tip) = pending_fp(&a.adopt_dry_run());
    a.advance_base(|root| fs::write(root.join("src/other.rs"), "pub fn o() {}\n").unwrap());
    let new_tip = a.tip();
    assert_ne!(new_tip, tip);

    let (status, fp_adopt, _) = a.lock_fp_on_adopt_wt(&new_tip);
    assert_eq!(status, LockStatus::Blocked);
    assert_eq!(
        fp_adopt, fp_dry,
        "an unrelated base commit does not move lock_fp"
    );
    a.sign(&new_tip, &fp_adopt).expect("signed on the new tip");
    let out =
        a.fx.run_checked(&a.adopt_merge_req(), &a.gh, Some(&|| true));
    assert!(matches!(out, PrMergeOutcome::Merged { .. }), "{out:?}");
    assert_eq!(a.gh.merges(), 1);
}

#[test]
fn case2b_signing_against_the_stale_tip_also_merges() {
    // The lane may sign on the tip it dry-ran against while base moves after.
    let a = setup();
    let (fp_dry, tip) = pending_fp(&a.adopt_dry_run());
    a.sign(&tip, &fp_dry).expect("signed");
    a.advance_base(|root| fs::write(root.join("src/other.rs"), "pub fn o() {}\n").unwrap());
    let out =
        a.fx.run_checked(&a.adopt_merge_req(), &a.gh, Some(&|| true));
    assert!(matches!(out, PrMergeOutcome::Merged { .. }), "{out:?}");
}

// Case 3 ---------------------------------------------------------------------

#[test]
fn case3_base_changing_the_locked_file_after_signing_blocks_the_merge() {
    let a = setup();
    let (fp_dry, tip) = pending_fp(&a.adopt_dry_run());
    a.sign(&tip, &fp_dry).expect("signed");
    a.advance_base(|root| {
        fs::write(
            root.join("Cargo.toml"),
            padded_cargo("# base touched\n", ""),
        )
        .unwrap();
    });
    let out =
        a.fx.run_checked(&a.adopt_merge_req(), &a.gh, Some(&|| true));
    eprintln!("CASE3 {out:?}");
    assert_eq!(code_of(&out), "cross_approval_required");
    assert_eq!(a.gh.merges(), 0, "nothing was merged");
}

// Case 4 ---------------------------------------------------------------------

#[test]
fn case4_no_command_at_all_for_a_locked_pr() {
    let a = setup_with(Fx::new(""));
    let out = a.adopt_dry_run();
    eprintln!("CASE4 {out:?}");
    match &out {
        PrMergeOutcome::Blocked { code, reason, .. } => {
            eprintln!("CASE4 code={code} reason={reason}");
        }
        other => panic!("a locked PR with no command must not be pending: {other:?}"),
    }
    assert_eq!(a.gh.merges(), 0);
}

#[test]
fn case4b_a_failing_command_on_a_locked_pr_has_the_same_code_as_no_command() {
    // `cross_approval_required` is chosen from the head-lock status alone, so
    // the lane cannot tell "tests failed" from "no tests" by the code.
    let a = setup_with(Fx::new("false"));
    let out = a.adopt_dry_run();
    let PrMergeOutcome::Blocked {
        code,
        receipt: Some(receipt),
        ..
    } = &out
    else {
        panic!("{out:?}");
    };
    eprintln!("CASE4b code={code} commands={:?}", receipt.commands);
    assert_eq!(code, "cross_approval_required");
    assert!(receipt.commands.iter().any(|c| !c.passed()));
}

// Case 5 ---------------------------------------------------------------------

#[test]
fn case5_an_old_approval_makes_a_plain_dry_run_clear_but_evaluate_still_sees_the_lock() {
    let a = setup();
    let (fp_dry, tip) = pending_fp(&a.adopt_dry_run());
    // A human (or an earlier run) signed exactly this lock_fp.
    a.sign(&tip, &fp_dry).expect("signed");

    let plain = a.fx.req(7, &a.head, true);
    let out = a.fx.run(&plain, &a.gh);
    eprintln!("CASE5 plain dry run: {out:?}");
    assert!(matches!(out, PrMergeOutcome::DryRunClear { .. }), "{out:?}");

    // `evaluate` on the adopt worktree reads the approval ...
    let rep = head_lock::evaluate(a.data(), &a.wt, &a.trusted_cfg(&tip), &a.merge_base(&tip));
    assert_eq!(rep.status, LockStatus::Approved);
    // ... but still lists the locked change, which is what the lane judges by.
    assert!(!rep.locked_changes.is_empty());
    assert!(rep.locked_changes.iter().any(|c| c.tier == LockTier::Hard));
    assert_eq!(rep.lock_fp, fp_dry);
}
