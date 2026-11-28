#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `aaf board adopt` step 2, merge side: `no_receipt_reuse` and
//! `run_commands_despite_head_lock` (`DryRunPendingHeadLock`). Each flag has a
//! test with it off (the old behaviour) and one with it on.

mod adopt_step2_support;

use std::fs;

use aaf_domain::gate::{GateStatus, LockStatus};
use aaf_engine::pr_ops::PrMergeOutcome;
use adopt_step2_support::{FakeGh, Fx, code_of};

// no_receipt_reuse ----------------------------------------------------------

#[test]
fn without_no_receipt_reuse_a_second_dry_run_reuses_the_common_receipt() {
    let fx = Fx::counting();
    let head = fx.edit_lib("feat/a");
    let gh = FakeGh::new(1, "feat/a", &head);
    let req = fx.req(1, &head, true);
    assert!(matches!(
        fx.run(&req, &gh),
        PrMergeOutcome::DryRunClear { .. }
    ));
    assert_eq!(fx.ran(), 1);
    assert!(matches!(
        fx.run(&req, &gh),
        PrMergeOutcome::DryRunClear { .. }
    ));
    assert_eq!(fx.ran(), 1, "the signed common receipt answered");
    let PrMergeOutcome::DryRunClear { receipt, .. } = fx.run(&req, &gh) else {
        panic!("clear dry run");
    };
    let common_id = aaf_engine::gate::stats_repo_id(fx.work.path());
    let common = aaf_fp::common_receipt_path(fx.data.path(), &common_id, &receipt.tree_fp);
    assert!(
        common.exists(),
        "a default run publishes the common receipt"
    );
}

#[test]
fn no_receipt_reuse_runs_the_commands_every_time_and_publishes_nothing() {
    let fx = Fx::counting();
    let head = fx.edit_lib("feat/a");
    let gh = FakeGh::new(1, "feat/a", &head);
    let mut req = fx.req(1, &head, true);
    req.no_receipt_reuse = true;
    let PrMergeOutcome::DryRunClear { receipt, .. } = fx.run(&req, &gh) else {
        panic!("clear dry run");
    };
    assert!(matches!(
        fx.run(&req, &gh),
        PrMergeOutcome::DryRunClear { .. }
    ));
    assert_eq!(fx.ran(), 2, "no receipt answered either run");
    // Nothing was published for other worktrees to find.
    let common_id = aaf_engine::gate::stats_repo_id(fx.work.path());
    let common = aaf_fp::common_receipt_path(fx.data.path(), &common_id, &receipt.tree_fp);
    assert!(!common.exists(), "{}", common.display());
}

#[test]
fn no_receipt_reuse_ignores_a_receipt_a_chat_gate_published() {
    let fx = Fx::counting();
    let head = fx.edit_lib("feat/a");
    let gh = FakeGh::new(1, "feat/a", &head);
    // The way `aaf merge` publishes a receipt for this exact merged tree.
    let default = fx.req(1, &head, true);
    fx.run(&default, &gh);
    assert_eq!(fx.ran(), 1);
    let mut adopt = fx.req(1, &head, true);
    adopt.no_receipt_reuse = true;
    fx.run(&adopt, &gh);
    assert_eq!(fx.ran(), 2);
}

// run_commands_despite_head_lock ----------------------------------------------

#[test]
fn without_the_flag_a_head_lock_is_cross_approval_required_and_no_command_runs() {
    let fx = Fx::counting();
    let head = fx.lock_cargo("feat/lock");
    let gh = FakeGh::new(2, "feat/lock", &head);
    let out = fx.run(&fx.req(2, &head, true), &gh);
    assert_eq!(code_of(&out), "cross_approval_required");
    assert_eq!(fx.ran(), 0);
}

#[test]
fn the_flag_reports_a_head_lock_that_is_the_only_block_as_pending() {
    let fx = Fx::counting();
    let head = fx.lock_cargo("feat/lock");
    let gh = FakeGh::new(2, "feat/lock", &head);
    let mut req = fx.req(2, &head, true);
    req.run_commands_despite_head_lock = true;
    let out = fx.run(&req, &gh);
    let PrMergeOutcome::DryRunPendingHeadLock {
        pr_number,
        head_sha,
        receipt,
        lock_fp,
        ..
    } = out
    else {
        panic!("{out:?}");
    };
    assert_eq!((pr_number, head_sha.as_str()), (2, head.as_str()));
    assert_eq!(fx.ran(), 1, "the commands really ran");
    assert_eq!(receipt.status, GateStatus::Blocked);
    assert_eq!(receipt.head_lock.status, LockStatus::Blocked);
    assert_eq!(receipt.head_lock.lock_fp, lock_fp);
    assert!(!lock_fp.is_empty());
    assert!(!receipt.commands.is_empty() && receipt.commands.iter().all(|c| c.passed()));
    assert_eq!(gh.merges(), 0);
    // It is never reused: a second run runs the commands again.
    fx.run(&req, &gh);
    assert_eq!(fx.ran(), 2);
}

#[test]
fn the_flag_does_nothing_without_dry_run() {
    let fx = Fx::counting();
    let head = fx.lock_cargo("feat/lock");
    let gh = FakeGh::new(2, "feat/lock", &head);
    let mut req = fx.req(2, &head, false);
    req.run_commands_despite_head_lock = true;
    let out = fx.run(&req, &gh);
    assert_eq!(code_of(&out), "cross_approval_required");
    assert_eq!(fx.ran(), 0);
    assert_eq!(gh.merges(), 0);
}

#[test]
fn a_failing_command_beside_the_head_lock_is_not_pending() {
    let fx = Fx::new("echo ran >> '{LOG}'; exit 1");
    let head = fx.lock_cargo("feat/lock");
    let gh = FakeGh::new(2, "feat/lock", &head);
    let mut req = fx.req(2, &head, true);
    req.run_commands_despite_head_lock = true;
    let out = fx.run(&req, &gh);
    let PrMergeOutcome::Blocked { code, receipt, .. } = out else {
        panic!("{out:?}");
    };
    assert_eq!(code, "cross_approval_required");
    let receipt = receipt.unwrap();
    assert!(receipt.commands.iter().any(|c| !c.passed()));
    assert_eq!(fx.ran(), 1);
}

#[test]
fn another_reason_beside_the_head_lock_is_not_pending() {
    let fx = Fx::counting();
    let leaked = ["AKIA", "IOSFODNN7EXAMPLE"].concat();
    let head = fx.branch("feat/lock", |root| {
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"adopt\"\nversion = \"0.2.0\"\n",
        )
        .unwrap();
        fs::write(
            root.join("src/extra.rs"),
            format!("const K: &str = \"{leaked}\";\n"),
        )
        .unwrap();
    });
    let gh = FakeGh::new(2, "feat/lock", &head);
    let mut req = fx.req(2, &head, true);
    req.run_commands_despite_head_lock = true;
    let out = fx.run(&req, &gh);
    assert_eq!(code_of(&out), "cross_approval_required");
    assert_eq!(fx.ran(), 0, "a critical finding stops the commands");
}

#[test]
fn no_test_command_beside_the_head_lock_is_not_pending() {
    let fx = Fx::new("");
    let head = fx.lock_cargo("feat/lock");
    let gh = FakeGh::new(2, "feat/lock", &head);
    let mut req = fx.req(2, &head, true);
    req.run_commands_despite_head_lock = true;
    let out = fx.run(&req, &gh);
    assert_eq!(code_of(&out), "cross_approval_required");
}

#[test]
fn a_proven_oracle_beside_the_head_lock_is_still_pending() {
    // The test command fails when the new test file sits beside the OLD `src/lib.rs`
    // and passes otherwise, so the oracle (revert the production change) proves red->green.
    let fx = Fx::with_gate(
        "if [ -f tests/new_test.rs ] && ! grep -q 'i32 { 2 }' src/lib.rs; then \
         echo 'running 1 test'; echo 'test result: FAILED. 0 passed; 1 failed'; exit 1; \
         else echo 'running 1 test'; echo 'test result: ok. 1 passed; 0 failed'; fi",
        "red_green_oracle = \"block\"",
    );
    let head = fx.branch("feat/lock", |root| {
        fs::write(root.join("src/lib.rs"), "pub fn v() -> i32 { 2 }\n").unwrap();
        fs::write(
            root.join("tests/new_test.rs"),
            "#[test]\nfn t() { assert_eq!(2, 2); }\n",
        )
        .unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"adopt\"\nversion = \"0.2.0\"\n",
        )
        .unwrap();
    });
    let gh = FakeGh::new(2, "feat/lock", &head);
    let mut req = fx.req(2, &head, true);
    req.run_commands_despite_head_lock = true;
    let out = fx.run(&req, &gh);
    let PrMergeOutcome::DryRunPendingHeadLock { receipt, .. } = out else {
        panic!("{out:?}");
    };
    assert!(
        receipt
            .reasons
            .iter()
            .any(|r| r == "oracle: red_green=proven"),
        "{:?}",
        receipt.reasons
    );
}

#[test]
fn the_flag_changes_nothing_for_a_pr_with_no_lock() {
    let fx = Fx::counting();
    let head = fx.edit_lib("feat/a");
    let gh = FakeGh::new(3, "feat/a", &head);
    let mut req = fx.req(3, &head, true);
    req.run_commands_despite_head_lock = true;
    let PrMergeOutcome::DryRunClear { receipt, .. } = fx.run(&req, &gh) else {
        panic!("a clear dry run stays DryRunClear");
    };
    assert_eq!(receipt.status, GateStatus::Clear);
}
