//! The two gate options `aaf board adopt` turns on: `no_receipt_reuse` and
//! `run_commands_despite_head_lock`. Each has an off test (the old behaviour,
//! unchanged) and an on test.

use std::fs;
use std::path::Path;

use aaf_domain::config::RepoConfig;
use aaf_domain::gate::{GateMode, GateStatus, LockStatus, Receipt};
use tempfile::TempDir;

use super::super::{GateRun, GateRunOptions, run_gate_with_options};
use super::support::{FixedClock, Repo};
use crate::throttle::tests::support::Rig;

fn counting_cfg(log: &Path) -> RepoConfig {
    let step = |name: &str| format!("echo {name} >> '{}'", log.display());
    let mut cfg = RepoConfig::default();
    cfg.commands.lint = step("lint");
    cfg.commands.test_scoped = step("scoped");
    cfg
}

struct Case {
    repo: Repo,
    out: TempDir,
    cfg: RepoConfig,
}

impl Case {
    fn new() -> Case {
        let out = TempDir::new().unwrap();
        let cfg = counting_cfg(&out.path().join("log"));
        Case {
            repo: Repo::new(),
            out,
            cfg,
        }
    }

    fn run(&self, now: i64, opts: GateRunOptions<'_>) -> GateRun {
        let rig = Rig::calm();
        run_gate_with_options(
            self.repo.data_dir(),
            self.repo.root(),
            &self.cfg,
            GateMode::Scoped,
            &FixedClock(now),
            &rig.throttle(),
            opts,
        )
        .unwrap()
    }

    fn ran(&self) -> Vec<String> {
        fs::read_to_string(self.out.path().join("log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn common_receipt_exists(&self, receipt: &Receipt) -> bool {
        let root = fs::canonicalize(self.repo.root()).unwrap();
        let id = super::super::stats_repo_id(&root);
        aaf_fp::common_receipt_path(self.repo.data_dir(), &id, &receipt.tree_fp).exists()
    }

    fn stored_receipt(&self) -> Receipt {
        let root = fs::canonicalize(self.repo.root()).unwrap();
        let key = aaf_fp::load_key(self.repo.data_dir()).unwrap();
        aaf_fp::read_receipt(self.repo.data_dir(), &key, &root).unwrap()
    }
}

fn publishing() -> GateRunOptions<'static> {
    GateRunOptions {
        allow_common_receipt_write: true,
        ..GateRunOptions::default()
    }
}

#[test]
fn without_the_flag_a_stored_receipt_is_reused() {
    let case = Case::new();
    let first = case.run(1_000, publishing());
    assert_eq!(first.receipt.status, GateStatus::Clear);
    assert!(!first.reused);
    let again = case.run(2_000, publishing());
    assert!(again.reused);
    assert_eq!(again.receipt, first.receipt);
    assert_eq!(case.ran(), vec!["lint", "scoped"]);
}

#[test]
fn without_the_flag_a_trusted_run_publishes_the_common_receipt() {
    let case = Case::new();
    let first = case.run(1_000, publishing());
    assert!(case.common_receipt_exists(&first.receipt));
}

#[test]
fn no_receipt_reuse_runs_the_commands_again_despite_a_stored_receipt() {
    let case = Case::new();
    case.run(1_000, publishing());
    assert_eq!(case.ran(), vec!["lint", "scoped"]);
    let again = case.run(
        2_000,
        GateRunOptions {
            no_receipt_reuse: true,
            ..publishing()
        },
    );
    assert!(!again.reused);
    assert_eq!(again.receipt.created_at, 2_000);
    assert_eq!(case.ran(), vec!["lint", "scoped", "lint", "scoped"]);
}

#[test]
fn no_receipt_reuse_publishes_no_common_receipt_even_when_allowed() {
    let case = Case::new();
    let run = case.run(
        1_000,
        GateRunOptions {
            no_receipt_reuse: true,
            ..publishing()
        },
    );
    assert_eq!(run.receipt.status, GateStatus::Clear);
    assert!(!case.common_receipt_exists(&run.receipt));
}

#[test]
fn no_receipt_reuse_ignores_a_common_receipt_a_chat_run_left_behind() {
    let case = Case::new();
    let chat = case.run(1_000, publishing());
    assert!(case.common_receipt_exists(&chat.receipt));
    // Another worktree's view: drop the per-worktree receipt, keep the common one.
    let root = fs::canonicalize(case.repo.root()).unwrap();
    fs::remove_file(aaf_fp::receipt_path(case.repo.data_dir(), &root)).unwrap();
    let reused = case.run(2_000, publishing());
    assert!(reused.reused, "the common receipt answers a default run");
    let adopt = case.run(
        3_000,
        GateRunOptions {
            no_receipt_reuse: true,
            ..publishing()
        },
    );
    assert!(!adopt.reused);
    assert_eq!(case.ran().len(), 4);
}

fn despite() -> GateRunOptions<'static> {
    GateRunOptions {
        run_commands_despite_head_lock: true,
        ..GateRunOptions::default()
    }
}

/// An edit of an existing test file: the head lock blocks until approved.
fn lock_the_tree(case: &Case) {
    case.repo.append("tests/a_test.rs", "// edited\n");
}

#[test]
fn without_the_flag_a_head_lock_skips_the_commands() {
    let case = Case::new();
    lock_the_tree(&case);
    let run = case.run(1_000, GateRunOptions::default());
    assert_eq!(run.receipt.status, GateStatus::Blocked);
    assert!(run.receipt.commands.is_empty());
    assert!(
        run.receipt
            .reasons
            .iter()
            .any(|r| r.starts_with("commands skipped:")),
        "{:?}",
        run.receipt.reasons
    );
    assert!(case.ran().is_empty());
}

#[test]
fn the_flag_runs_the_commands_but_the_receipt_stays_blocked_and_is_not_shared() {
    let case = Case::new();
    lock_the_tree(&case);
    let run = case.run(
        1_000,
        GateRunOptions {
            allow_common_receipt_write: true,
            ..despite()
        },
    );
    assert_eq!(case.ran(), vec!["lint", "scoped"]);
    let receipt = &run.receipt;
    assert_eq!(receipt.status, GateStatus::Blocked);
    assert_eq!(receipt.head_lock.status, LockStatus::Blocked);
    assert!(!receipt.head_lock.lock_fp.is_empty());
    assert!(
        receipt.reasons[0].starts_with("head lock:"),
        "{:?}",
        receipt.reasons
    );
    assert!(
        !receipt
            .reasons
            .iter()
            .any(|r| r.starts_with("commands skipped:")),
        "{:?}",
        receipt.reasons
    );
    assert!(!receipt.commands.is_empty() && receipt.commands.iter().all(|c| c.passed()));
    // What was persisted is the Blocked receipt, and nothing is published.
    assert_eq!(case.stored_receipt().status, GateStatus::Blocked);
    assert!(!case.common_receipt_exists(receipt));
    // Never reused: a second run runs the commands again.
    case.run(2_000, despite());
    assert_eq!(case.ran().len(), 4);
}

#[test]
fn the_flag_still_blocks_on_a_failing_command_and_says_why() {
    let mut case = Case::new();
    let log = case.out.path().join("log");
    case.cfg.commands.test_scoped = format!("echo scoped >> '{}'; exit 1", log.display());
    lock_the_tree(&case);
    let run = case.run(1_000, despite());
    assert_eq!(run.receipt.status, GateStatus::Blocked);
    assert!(run.receipt.reasons[0].starts_with("head lock:"));
    assert!(
        run.receipt
            .reasons
            .iter()
            .any(|r| r.starts_with("command ") && r.contains("failed")),
        "{:?}",
        run.receipt.reasons
    );
}

#[test]
fn the_flag_does_not_hide_a_critical_static_finding() {
    let case = Case::new();
    lock_the_tree(&case);
    case.repo.write(
        "src/key.rs",
        &format!("const K: &str = \"{}\";\n", super::support::fake_secret()),
    );
    let run = case.run(1_000, despite());
    assert_eq!(run.receipt.status, GateStatus::Blocked);
    assert!(
        run.receipt
            .reasons
            .iter()
            .any(|r| r.starts_with("static checks:")),
        "{:?}",
        run.receipt.reasons
    );
    // The earlier stage blocks, so no command ran.
    assert!(case.ran().is_empty());
}

#[test]
fn the_flag_changes_nothing_when_no_lock_blocks() {
    let case = Case::new();
    case.repo
        .write("src/more.rs", "pub fn two() -> u32 {\n    2\n}\n");
    let run = case.run(1_000, despite());
    assert_eq!(
        run.receipt.status,
        GateStatus::Clear,
        "{:?}",
        run.receipt.reasons
    );
    assert_eq!(run.receipt.head_lock.status, LockStatus::Clear);
    assert!(
        run.receipt
            .reasons
            .iter()
            .all(|r| !r.starts_with("head lock:"))
    );
}
