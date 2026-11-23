#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Integration tests for v3.3 Phase 3: native locked PR merge gate (`aaf merge`).

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use aaf_domain::config::{MachineConfig, RepoConfig};
use aaf_domain::gate::GateMode;
use aaf_domain::ports::Clock;
use aaf_engine::gate::docs_fast::DOCS_FAST_PATH_NOTE;
use aaf_engine::gate::head_lock;
use aaf_engine::git::{
    GhClient, GitError, MergeStrategy, PrCheck, PrInfo, PrMergeOutcome, PrMergeRequest, merge_pr,
};
use tempfile::TempDir;

struct FixedClock(i64);
impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

struct FakeGh {
    pr: PrInfo,
    checks: Vec<PrCheck>,
    merged_calls: AtomicUsize,
}

impl GhClient for FakeGh {
    fn view_pr(&self, _repo_root: &Path, _pr_number: u64) -> Result<PrInfo, GitError> {
        Ok(self.pr.clone())
    }

    fn pr_checks(&self, _repo_root: &Path, _pr_number: u64) -> Result<Vec<PrCheck>, GitError> {
        Ok(self.checks.clone())
    }

    fn merge_pr(
        &self,
        _repo_root: &Path,
        _pr_number: u64,
        _expect_head: &str,
        _strategy: MergeStrategy,
        _subject: Option<&str>,
        _body_file: Option<&Path>,
    ) -> Result<(), GitError> {
        self.merged_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct Fixture {
    _bare: TempDir,
    work: TempDir,
    data: TempDir,
}

impl Fixture {
    fn new(test_scoped: &str) -> Fixture {
        let bare = TempDir::new().unwrap();
        git(bare.path(), &["init", "--bare", "-b", "main"]);

        let work = TempDir::new().unwrap();
        git(work.path(), &["init", "-b", "main"]);
        git(work.path(), &["config", "user.email", "dev@example.com"]);
        git(work.path(), &["config", "user.name", "Dev"]);
        git(
            work.path(),
            &["remote", "add", "origin", bare.path().to_str().unwrap()],
        );

        fs::create_dir_all(work.path().join(".autofactory")).unwrap();
        fs::create_dir_all(work.path().join("src")).unwrap();
        fs::create_dir_all(work.path().join("tests")).unwrap();
        fs::create_dir_all(work.path().join("docs")).unwrap();

        let cfg = format!(
            r#"[project]
name = "demo"
profile = "rust"

[commands]
test_scoped = "{test_scoped}"
test_full = "true"

[gate]
base_ref = "origin/main"
docs_fast_path = true

[cross_audit]
require_different_provider = false
"#
        );
        fs::write(work.path().join(".autofactory/config.toml"), cfg).unwrap();
        fs::write(work.path().join("src/lib.rs"), "pub fn v() -> i32 { 1 }\n").unwrap();
        fs::write(
            work.path().join("tests/basic_test.rs"),
            "#[test] fn t() { assert_eq!(1, 1); }\n",
        )
        .unwrap();
        fs::write(work.path().join("docs/plan.md"), "# Plan\n").unwrap();
        git(work.path(), &["add", "."]);
        git(work.path(), &["commit", "-m", "initial"]);
        git(work.path(), &["push", "-u", "origin", "main"]);

        Fixture {
            _bare: bare,
            work,
            data: TempDir::new().unwrap(),
        }
    }

    fn make_pr_branch(&self, branch: &str, edit: impl FnOnce(&Path)) -> String {
        git(self.work.path(), &["checkout", "-b", branch, "origin/main"]);
        edit(self.work.path());
        git(self.work.path(), &["add", "-A"]);
        git(self.work.path(), &["commit", "-m", branch]);
        let sha = git(self.work.path(), &["rev-parse", "HEAD"]);
        git(self.work.path(), &["push", "-u", "origin", branch]);
        git(self.work.path(), &["checkout", "main"]);
        sha
    }

    fn req(&self, pr_number: u64, expect_head: &str, dry_run: bool) -> PrMergeRequest {
        PrMergeRequest {
            repo_root: self.work.path().to_path_buf(),
            data_dir: self.data.path().to_path_buf(),
            pr_number,
            expect_head: expect_head.to_string(),
            remote: "origin".to_string(),
            mode: GateMode::Scoped,
            strategy: MergeStrategy::Squash,
            subject: None,
            body_file: None,
            ci_wait_secs: 0,
            lock_timeout: Duration::from_secs(10),
            dry_run,
            machine: MachineConfig::default(),
        }
    }
}

fn pr_info(number: u64, branch: &str, head: &str) -> PrInfo {
    PrInfo {
        number,
        state: "OPEN".to_string(),
        head_ref_oid: head.to_string(),
        head_ref_name: branch.to_string(),
        base_ref_name: "main".to_string(),
        mergeable: "MERGEABLE".to_string(),
        is_draft: false,
        title: format!("PR #{number}"),
        url: format!("https://github.com/org/repo/pull/{number}"),
    }
}

#[test]
fn merge_blocks_head_mismatch_and_unapproved_or_same_account_test_edit() {
    let fx = Fixture::new("true");
    let head = fx.make_pr_branch("feat/edit-test", |root| {
        fs::write(
            root.join("tests/basic_test.rs"),
            "#[test] fn t() { assert_eq!(2, 2); }\n",
        )
        .unwrap();
    });
    let noop = |_: &aaf_engine::throttle::Progress<'_>| {};

    // 1. Head mismatch is rejected before any gate or merge.
    let wrong_sha = "0".repeat(40);
    let gh = FakeGh {
        pr: pr_info(10, "feat/edit-test", &head),
        checks: vec![],
        merged_calls: AtomicUsize::new(0),
    };
    let out = merge_pr(&fx.req(10, &wrong_sha, false), &gh, &FixedClock(1), &noop).unwrap();
    match out {
        PrMergeOutcome::Blocked { code, .. } => assert_eq!(code, "head_mismatch"),
        other => panic!("expected head_mismatch, got {other:?}"),
    }
    assert_eq!(gh.merged_calls.load(Ordering::SeqCst), 0);

    // 2. Edited existing test without cross-approval -> Blocked(cross_approval_required).
    let out = merge_pr(&fx.req(10, &head, false), &gh, &FixedClock(2), &noop).unwrap();
    match out {
        PrMergeOutcome::Blocked { code, .. } => assert_eq!(code, "cross_approval_required"),
        other => panic!("expected cross_approval_required, got {other:?}"),
    }
    assert_eq!(gh.merged_calls.load(Ordering::SeqCst), 0);

    // 3. Signing a same_account=true approval MUST STILL BE REJECTED during merge
    // (even though repo config had require_different_provider = false).
    git(fx.work.path(), &["checkout", "feat/edit-test"]);
    let mut cfg = RepoConfig::default();
    cfg.gate.base_ref = "origin/main".to_string();
    head_lock::approve(
        fx.data.path(),
        fx.work.path(),
        &cfg,
        "self-author",
        true,
        &FixedClock(3),
    )
    .unwrap();
    git(fx.work.path(), &["checkout", "main"]);
    let out = merge_pr(&fx.req(10, &head, false), &gh, &FixedClock(4), &noop).unwrap();
    match out {
        PrMergeOutcome::Blocked { code, .. } => assert_eq!(code, "cross_approval_required"),
        other => panic!("same_account approval must be blocked, got {other:?}"),
    }

    // 4. Independent approval (same_account = false) unlocks the exact lock_fp and merges!
    git(fx.work.path(), &["checkout", "feat/edit-test"]);
    head_lock::approve(
        fx.data.path(),
        fx.work.path(),
        &cfg,
        "independent-auditor",
        false,
        &FixedClock(5),
    )
    .unwrap();
    git(fx.work.path(), &["checkout", "main"]);
    let out = merge_pr(&fx.req(10, &head, false), &gh, &FixedClock(6), &noop).unwrap();
    match out {
        PrMergeOutcome::Merged { pr_number, .. } => assert_eq!(pr_number, 10),
        other => panic!("expected Merged with independent approval, got {other:?}"),
    }
    assert_eq!(gh.merged_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn merge_ignores_pr_config_tampering_and_uses_docs_fast_path_for_docs_pr() {
    let fx = Fixture::new("exit 1");
    let noop = |_: &aaf_engine::throttle::Progress<'_>| {};

    // 1. Adversarial PR weakening .autofactory/config.toml on PR branch is ignored and blocked.
    let evil_head = fx.make_pr_branch("feat/tamper", |root| {
        fs::write(
            root.join(".autofactory/config.toml"),
            "[commands]\ntest_scoped = \"true\"\n[gate]\nhead_lock_globs = []\n",
        )
        .unwrap();
    });
    let gh_evil = FakeGh {
        pr: pr_info(11, "feat/tamper", &evil_head),
        checks: vec![],
        merged_calls: AtomicUsize::new(0),
    };
    let out = merge_pr(
        &fx.req(11, &evil_head, false),
        &gh_evil,
        &FixedClock(10),
        &noop,
    )
    .unwrap();
    match out {
        PrMergeOutcome::Blocked { code, .. } => assert_eq!(code, "cross_approval_required"),
        other => panic!("tampered config PR must be blocked, got {other:?}"),
    }
    assert_eq!(gh_evil.merged_calls.load(Ordering::SeqCst), 0);

    // 2. Clean docs-only PR passes dry-run via docs_fast_path (even though test_scoped="exit 1").
    let docs_head = fx.make_pr_branch("docs/progress", |root| {
        fs::write(root.join("docs/plan.md"), "# Plan\n- Total: 90%\n").unwrap();
    });
    let gh_docs = FakeGh {
        pr: pr_info(563, "docs/progress", &docs_head),
        checks: vec![],
        merged_calls: AtomicUsize::new(0),
    };
    let out = merge_pr(
        &fx.req(563, &docs_head, true),
        &gh_docs,
        &FixedClock(11),
        &noop,
    )
    .unwrap();
    match out {
        PrMergeOutcome::DryRunClear {
            pr_number, receipt, ..
        } => {
            assert_eq!(pr_number, 563);
            assert!(
                receipt
                    .reasons
                    .iter()
                    .any(|r| r.starts_with(DOCS_FAST_PATH_NOTE)),
                "{:?}",
                receipt.reasons
            );
        }
        other => panic!("expected DryRunClear via docs_fast_path, got {other:?}"),
    }
    assert_eq!(gh_docs.merged_calls.load(Ordering::SeqCst), 0);
}
