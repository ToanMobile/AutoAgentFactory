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

#[path = "../../aaf-fp/tests/exec_support.rs"]
mod exec_support;
use exec_support::write_executable;

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
            // The CPU-load wait is off: the host running the tests may be busy.
            machine: MachineConfig {
                max_load_percent: 0,
                ..MachineConfig::default()
            },
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

#[test]
fn approval_unlocks_merge_when_pr_tightens_head_lock_globs_and_edits_newly_locked_doc() {
    let fx = Fixture::new("true");
    // PR adds `docs/plan.md` to head_lock_globs in `.autofactory/config.toml`
    // AND modifies `docs/plan.md`. Approving on the PR branch uses the PR's
    // config (2 locked files), while `aaf merge` evaluates against origin/main's
    // trusted base config (1 locked file: `.autofactory/config.toml`).
    let head = fx.make_pr_branch("chore/tighten-lock", |root| {
        let cfg = fs::read_to_string(root.join(".autofactory/config.toml")).unwrap();
        let updated = cfg.replace(
            "docs_fast_path = true",
            "docs_fast_path = true\nhead_lock_globs = [\".autofactory/config.toml\", \"docs/plan.md\"]",
        );
        fs::write(root.join(".autofactory/config.toml"), updated).unwrap();
        fs::write(root.join("docs/plan.md"), "# Plan\n- Tightened lock\n").unwrap();
    });

    // Sign approval on the PR branch using the PR branch's own config (as `aaf approve` does).
    git(fx.work.path(), &["checkout", "chore/tighten-lock"]);
    let pr_cfg: RepoConfig = toml::from_str(
        &fs::read_to_string(fx.work.path().join(".autofactory/config.toml")).unwrap(),
    )
    .unwrap();
    head_lock::approve(
        fx.data.path(),
        fx.work.path(),
        &pr_cfg,
        "independent-reviewer",
        false,
        &FixedClock(20),
    )
    .unwrap();
    git(fx.work.path(), &["checkout", "main"]);

    let gh = FakeGh {
        pr: pr_info(566, "chore/tighten-lock", &head),
        checks: vec![],
        merged_calls: AtomicUsize::new(0),
    };
    let noop = |_: &aaf_engine::throttle::Progress<'_>| {};
    let out = merge_pr(&fx.req(566, &head, false), &gh, &FixedClock(21), &noop).unwrap();
    match out {
        PrMergeOutcome::Merged { pr_number, .. } => assert_eq!(pr_number, 566),
        other => panic!("expected Merged when PR tightened head_lock_globs, got {other:?}"),
    }
    assert_eq!(gh.merged_calls.load(Ordering::SeqCst), 1);
}

struct LaggingHeadGh {
    stale_head: String,
    fresh_head: String,
    branch: String,
    pr_number: u64,
    views: AtomicUsize,
    merged_calls: AtomicUsize,
}

impl GhClient for LaggingHeadGh {
    fn view_pr(&self, _repo_root: &Path, _pr_number: u64) -> Result<PrInfo, GitError> {
        let n = self.views.fetch_add(1, Ordering::SeqCst);
        let head = if n == 0 {
            &self.stale_head
        } else {
            &self.fresh_head
        };
        Ok(pr_info(self.pr_number, &self.branch, head))
    }

    fn pr_checks(&self, _repo_root: &Path, _pr_number: u64) -> Result<Vec<PrCheck>, GitError> {
        Ok(vec![])
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

#[test]
fn merge_waits_for_github_head_propagation_when_ci_wait_is_enabled() {
    let fx = Fixture::new("true");
    let head = fx.make_pr_branch("feat/lagging-head", |root| {
        fs::write(root.join("src/lib.rs"), "pub fn v() -> i32 { 42 }\n").unwrap();
    });
    let gh = LaggingHeadGh {
        stale_head: "1".repeat(40),
        fresh_head: head.clone(),
        branch: "feat/lagging-head".to_string(),
        pr_number: 568,
        views: AtomicUsize::new(0),
        merged_calls: AtomicUsize::new(0),
    };
    let mut req = fx.req(568, &head, false);
    req.ci_wait_secs = 5;
    let noop = |_: &aaf_engine::throttle::Progress<'_>| {};
    let out = merge_pr(&req, &gh, &FixedClock(30), &noop).unwrap();
    match out {
        PrMergeOutcome::Merged { pr_number, .. } => assert_eq!(pr_number, 568),
        other => panic!("expected Merged after GitHub head caught up, got {other:?}"),
    }
    assert!(gh.views.load(Ordering::SeqCst) >= 2);
    assert_eq!(gh.merged_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn approval_signed_with_untracked_agent_files_still_unlocks_clean_merge() {
    let fx = Fixture::new("true");
    let head = fx.make_pr_branch("feat/untracked-agent-files", |root| {
        fs::write(
            root.join("tests/basic_test.rs"),
            "#[test] fn t() { assert_eq!(3, 3); }\n",
        )
        .unwrap();
    });

    // Simulate a worktree that also has untracked local `.claude/` / `.agents/` files.
    git(fx.work.path(), &["checkout", "feat/untracked-agent-files"]);
    fs::create_dir_all(fx.work.path().join(".claude/hooks")).unwrap();
    fs::write(
        fx.work.path().join(".claude/hooks/local.sh"),
        "#!/bin/sh\necho local\n",
    )
    .unwrap();
    let mut cfg = RepoConfig::default();
    cfg.gate.base_ref = "origin/main".to_string();
    head_lock::approve(
        fx.data.path(),
        fx.work.path(),
        &cfg,
        "independent-reviewer",
        false,
        &FixedClock(40),
    )
    .unwrap();
    fs::remove_dir_all(fx.work.path().join(".claude")).unwrap();
    git(fx.work.path(), &["checkout", "main"]);

    let gh = FakeGh {
        pr: pr_info(567, "feat/untracked-agent-files", &head),
        checks: vec![],
        merged_calls: AtomicUsize::new(0),
    };
    let noop = |_: &aaf_engine::throttle::Progress<'_>| {};
    let out = merge_pr(&fx.req(567, &head, false), &gh, &FixedClock(41), &noop).unwrap();
    match out {
        PrMergeOutcome::Merged { pr_number, .. } => assert_eq!(pr_number, 567),
        other => {
            panic!("expected Merged despite untracked agent files during approve, got {other:?}")
        }
    }
    assert_eq!(gh.merged_calls.load(Ordering::SeqCst), 1);
}

#[cfg(unix)]
#[test]
fn a13_scratch_merge_never_runs_pr_controlled_hooks_from_relative_hooks_path() {
    let fx = Fixture::new("true");
    let marker = fx.data.path().join("pr-hook-ran");
    let marker_s = marker.to_str().unwrap().to_string();
    let head = fx.make_pr_branch("feat/hooked", |root| {
        fs::write(
            root.join("tests/basic_test.rs"),
            "#[test] fn t() { assert_eq!(4, 4); }\n",
        )
        .unwrap();
        fs::create_dir_all(root.join(".githooks")).unwrap();
        for hook in ["post-merge", "prepare-commit-msg", "post-checkout"] {
            let p = root.join(".githooks").join(hook);
            write_executable(
                &p,
                format!(
                    "#!/bin/sh\ntouch '{marker_s}'\n\
                     git show HEAD^1:tests/basic_test.rs > tests/basic_test.rs 2>/dev/null\n\
                     exit 0\n"
                ),
            );
        }
    });
    // A repo-relative hooksPath (husky-style) resolves inside the scratch
    // worktree, i.e. to files the PR itself controls.
    git(fx.work.path(), &["config", "core.hooksPath", ".githooks"]);
    let gh = FakeGh {
        pr: pr_info(569, "feat/hooked", &head),
        checks: vec![],
        merged_calls: AtomicUsize::new(0),
    };
    let noop = |_: &aaf_engine::throttle::Progress<'_>| {};
    let out = merge_pr(&fx.req(569, &head, true), &gh, &FixedClock(50), &noop).unwrap();
    assert!(
        !marker.exists(),
        "PR-controlled git hook ran inside the scratch merge worktree"
    );
    match out {
        PrMergeOutcome::Blocked { code, .. } => assert_eq!(code, "cross_approval_required"),
        other => panic!("locked test edit without approval must block, got {other:?}"),
    }
    assert_eq!(gh.merged_calls.load(Ordering::SeqCst), 0);
}
