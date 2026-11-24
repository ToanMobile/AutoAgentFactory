#![allow(clippy::unwrap_used, clippy::expect_used)]
//! A13 R10-L4-1: `aaf merge` layers the tighten-only overlays (tier 1 global,
//! tier 3 `.autofactory/config.local.toml`, `AAF_*` environment) over the
//! tier-2 config it reads from the immutable base commit, exactly as
//! `aaf gate` layers them over the working-tree file. Before the fix the
//! merge gate used the raw base tier-2 file alone, so a local
//! `red_green_oracle = "block"` or an extra head-lock glob was dropped.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use aaf_domain::config::MachineConfig;
use aaf_domain::gate::GateMode;
use aaf_domain::ports::Clock;
use aaf_engine::config::LOCAL_CONFIG_REL;
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
    merged_calls: AtomicUsize,
}

impl GhClient for FakeGh {
    fn view_pr(&self, _repo_root: &Path, _pr_number: u64) -> Result<PrInfo, GitError> {
        Ok(self.pr.clone())
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

/// A repo pushed to a bare origin, with a PR branch that changes only
/// `src/lib.rs` (no test). Returns (bare, work, data, PR head).
fn fixture() -> (TempDir, TempDir, TempDir, String) {
    let bare = TempDir::new().unwrap();
    git(bare.path(), &["init", "--bare", "-b", "main"]);
    let work = TempDir::new().unwrap();
    let root = work.path();
    git(root, &["init", "-b", "main"]);
    git(root, &["config", "user.email", "dev@example.com"]);
    git(root, &["config", "user.name", "Dev"]);
    git(
        root,
        &["remote", "add", "origin", bare.path().to_str().unwrap()],
    );
    fs::create_dir_all(root.join(".autofactory")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("tests")).unwrap();
    fs::write(
        root.join(".autofactory/config.toml"),
        "[project]\nname = \"demo\"\nprofile = \"rust\"\n\n[commands]\ntest_scoped = \"true\"\n\
         test_full = \"true\"\n\n[gate]\nbase_ref = \"origin/main\"\n\n[cross_audit]\n\
         require_different_provider = false\n",
    )
    .unwrap();
    fs::write(root.join(".gitignore"), "/.autofactory/config.local.toml\n").unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn v() -> i32 { 1 }\n").unwrap();
    fs::write(root.join("tests/basic_test.rs"), "#[test] fn t() {}\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "initial"]);
    git(root, &["push", "-u", "origin", "main"]);

    git(root, &["checkout", "-b", "feat/src", "origin/main"]);
    fs::write(root.join("src/lib.rs"), "pub fn v() -> i32 { 2 }\n").unwrap();
    git(root, &["commit", "-am", "change src"]);
    let head = git(root, &["rev-parse", "HEAD"]);
    git(root, &["push", "-u", "origin", "feat/src"]);
    git(root, &["checkout", "main"]);
    (bare, work, TempDir::new().unwrap(), head)
}

fn run_merge(work: &Path, data: &Path, head: &str) -> (PrMergeOutcome, usize) {
    let req = PrMergeRequest {
        repo_root: work.to_path_buf(),
        data_dir: data.to_path_buf(),
        pr_number: 7,
        expect_head: head.to_string(),
        remote: "origin".to_string(),
        mode: GateMode::Scoped,
        strategy: MergeStrategy::Squash,
        subject: None,
        body_file: None,
        ci_wait_secs: 0,
        lock_timeout: Duration::from_secs(10),
        dry_run: false,
        // The CPU-load wait is off: the host running the tests may be busy.
        machine: MachineConfig {
            max_load_percent: 0,
            ..MachineConfig::default()
        },
        global_config: None,
    };
    let gh = FakeGh {
        pr: PrInfo {
            number: 7,
            state: "OPEN".to_string(),
            head_ref_oid: head.to_string(),
            head_ref_name: "feat/src".to_string(),
            base_ref_name: "main".to_string(),
            mergeable: "MERGEABLE".to_string(),
            is_draft: false,
            title: "PR #7".to_string(),
            url: "https://github.com/org/repo/pull/7".to_string(),
        },
        merged_calls: AtomicUsize::new(0),
    };
    let noop = |_: &aaf_engine::throttle::Progress<'_>| {};
    let out = merge_pr(&req, &gh, &FixedClock(1), &noop).unwrap();
    (out, gh.merged_calls.load(Ordering::SeqCst))
}

#[test]
fn merge_without_overlays_merges_a_src_only_pr() {
    let (_bare, work, data, head) = fixture();
    let (out, merged) = run_merge(work.path(), data.path(), &head);
    assert!(matches!(out, PrMergeOutcome::Merged { .. }), "{out:?}");
    assert_eq!(merged, 1);
}

#[test]
fn merge_honours_a_local_head_lock_glob_over_the_base_config() {
    let (_bare, work, data, head) = fixture();
    fs::write(
        work.path().join(LOCAL_CONFIG_REL),
        "[gate]\nhead_lock_globs = [\"src/**\"]\n",
    )
    .unwrap();
    let (out, merged) = run_merge(work.path(), data.path(), &head);
    match out {
        PrMergeOutcome::Blocked { code, .. } => assert_eq!(code, "cross_approval_required"),
        other => panic!("a local head-lock glob must block the merge, got {other:?}"),
    }
    assert_eq!(merged, 0);
}

#[test]
fn merge_honours_a_local_red_green_oracle_block_over_the_base_config() {
    let (_bare, work, data, head) = fixture();
    fs::write(
        work.path().join(LOCAL_CONFIG_REL),
        "[gate]\nred_green_oracle = \"block\"\n",
    )
    .unwrap();
    let (out, merged) = run_merge(work.path(), data.path(), &head);
    match out {
        PrMergeOutcome::Blocked { code, .. } => assert_eq!(code, "gate_blocked"),
        other => panic!("a local red_green_oracle = block must block, got {other:?}"),
    }
    assert_eq!(merged, 0);
}
