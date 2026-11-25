#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Edge-case regression tests for WarmSlot hardening, GC worktree pruning,
//! intermediate symlink / nested `.git` rejection, unrelated histories in
//! `in_memory_merge_tree`, and post-CI-wait PR refresh.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use aaf_domain::config::MachineConfig;
use aaf_domain::gate::GateMode;
use aaf_domain::ports::Clock;
use aaf_engine::gate::stats_repo_id;
use aaf_engine::gc::{GcOptions, run_gc};
use aaf_engine::git::warm_slot::{
    SlotKind, WarmSlot, promote_clean_base_snapshot, warm_slots_root,
};
use aaf_engine::git::{GhClient, GitError, MergeStrategy, PrCheck, PrInfo};
use aaf_engine::pr_ops::{PrMergeOutcome, PrMergeRequest, merge_pr};
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
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "dev")
        .env("GIT_AUTHOR_EMAIL", "dev@example.invalid")
        .env("GIT_COMMITTER_NAME", "dev")
        .env("GIT_COMMITTER_EMAIL", "dev@example.invalid")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

struct RepoFixture {
    _bare: TempDir,
    work: TempDir,
    data: TempDir,
    counter_file: PathBuf,
    _counter_dir: TempDir,
}

impl RepoFixture {
    fn new() -> Self {
        let (bare, work, data, counter_dir) = (
            TempDir::new().unwrap(),
            TempDir::new().unwrap(),
            TempDir::new().unwrap(),
            TempDir::new().unwrap(),
        );
        let counter_file = counter_dir.path().join("runs.log");
        git(bare.path(), &["init", "--bare", "-b", "main"]);
        git(work.path(), &["init", "-b", "main"]);
        git(
            work.path(),
            &["remote", "add", "origin", bare.path().to_str().unwrap()],
        );
        fs::create_dir_all(work.path().join(".autofactory")).unwrap();
        fs::create_dir_all(work.path().join("src")).unwrap();
        let cfg = format!(
            "[project]\nname = \"demo\"\nprofile = \"rust\"\n\n\
             [commands]\ntest_scoped = \"sh -c 'echo run >> {}'\"\ntest_full = \"true\"\n\n\
             [gate]\nbase_ref = \"origin/main\"\ndocs_fast_path = true\n\n\
             [cross_audit]\nrequire_different_provider = false\n",
            counter_file.display()
        );
        fs::write(work.path().join(".autofactory/config.toml"), cfg).unwrap();
        fs::write(work.path().join(".gitignore"), "target/\n").unwrap();
        fs::write(work.path().join("src/lib.rs"), "pub fn v() -> i32 { 1 }\n").unwrap();
        git(work.path(), &["add", "-A"]);
        git(work.path(), &["commit", "-m", "initial"]);
        git(work.path(), &["push", "-u", "origin", "main"]);
        Self {
            _bare: bare,
            work,
            data,
            counter_file,
            _counter_dir: counter_dir,
        }
    }

    fn acquire_merge_slot(&self, base_sha: &str, tag: &str) -> WarmSlot {
        let common_id = stats_repo_id(self.work.path());
        WarmSlot::acquire(
            self.work.path(),
            self.data.path(),
            &common_id,
            SlotKind::Merge,
            base_sha,
            tag,
        )
        .unwrap()
    }

    fn runs(&self) -> usize {
        fs::read_to_string(&self.counter_file)
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    fn req(&self, pr_number: u64, expect_head: &str, ci_wait_secs: u64) -> PrMergeRequest {
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
            ci_wait_secs,
            require_checks: false,
            lock_timeout: Duration::from_secs(10),
            dry_run: false,
            machine: MachineConfig::default(),
            global_config: None,
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
        url: format!("https://example.invalid/pr/{number}"),
        ..Default::default()
    }
}

struct MockGh {
    pr: PrInfo,
    merge_after_first_view: bool,
    pending_first_check: bool,
    views: AtomicUsize,
    checks: AtomicUsize,
    merged: AtomicUsize,
}

impl GhClient for MockGh {
    fn view_pr(&self, _: &Path, _: u64) -> Result<PrInfo, GitError> {
        let n = self.views.fetch_add(1, Ordering::SeqCst);
        let mut info = self.pr.clone();
        if self.merge_after_first_view && n >= 1 {
            info.state = "MERGED".to_string();
        }
        Ok(info)
    }
    fn pr_checks(&self, _: &Path, _: u64) -> Result<Vec<PrCheck>, GitError> {
        if self.pending_first_check && self.checks.fetch_add(1, Ordering::SeqCst) == 0 {
            return Ok(vec![PrCheck {
                name: "ci".to_string(),
                state: "IN_PROGRESS".to_string(),
                bucket: "pending".to_string(),
            }]);
        }
        Ok(vec![])
    }
    fn merge_pr(
        &self,
        _: &Path,
        _: u64,
        _: &str,
        _: MergeStrategy,
        _: Option<&str>,
        _: Option<&Path>,
    ) -> Result<(), GitError> {
        self.merged.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[test]
fn warm_slot_drop_wipes_nested_git_repo_completely() {
    let fx = RepoFixture::new();
    let base_sha = git(fx.work.path(), &["rev-parse", "HEAD"]);
    let slot = fx.acquire_merge_slot(&base_sha, "nested-git");
    assert!(slot.is_persistent());
    let slot_path = slot.path().to_path_buf();

    let nested = slot_path.join("nested_repo");
    fs::create_dir_all(&nested).unwrap();
    git(&nested, &["init", "-b", "main"]);
    fs::write(nested.join("leak.txt"), b"untrusted-subrepo").unwrap();
    git(&nested, &["add", "leak.txt"]);
    git(&nested, &["commit", "-m", "nested"]);
    assert!(nested.join(".git").is_dir());

    drop(slot);
    assert!(
        !slot_path.join("nested_repo").exists(),
        "Drop must wipe nested git repositories via `git clean -ffdx`"
    );
}

#[cfg(unix)]
#[test]
fn warm_slot_recovers_from_readonly_dir_on_next_acquire() {
    use std::os::unix::fs::PermissionsExt;

    let fx = RepoFixture::new();
    let base_sha = git(fx.work.path(), &["rev-parse", "HEAD"]);
    let slot1 = fx.acquire_merge_slot(&base_sha, "ro-seed");
    let slot_path = slot1.path().to_path_buf();
    let ro_dir = slot_path.join("ro_dir");
    fs::create_dir_all(&ro_dir).unwrap();
    let ro_file = ro_dir.join("locked.txt");
    fs::write(&ro_file, b"readonly-artifact").unwrap();
    fs::set_permissions(&ro_file, fs::Permissions::from_mode(0o444)).unwrap();
    fs::set_permissions(&ro_dir, fs::Permissions::from_mode(0o555)).unwrap();
    drop(slot1);

    let slot2 = fx.acquire_merge_slot(&base_sha, "ro-recover");
    assert!(slot2.is_persistent());
    assert_eq!(slot2.path(), slot_path.as_path());
    assert!(!slot2.path().join("ro_dir").exists());
}

#[test]
fn promote_clean_base_snapshot_rejects_nested_dot_git_and_intermediate_symlink() {
    let fx = RepoFixture::new();
    let base_sha = git(fx.work.path(), &["rev-parse", "HEAD"]);
    let common_id = stats_repo_id(fx.work.path());
    let snaps_dir = warm_slots_root(fx.data.path())
        .join("merge")
        .join(&common_id)
        .join("base-snaps");
    let slot = fx.acquire_merge_slot(&base_sha, "snap-edges");

    assert!(
        promote_clean_base_snapshot(slot.path(), &snaps_dir, &base_sha, &["sub/.git"]).is_err()
    );
    assert!(
        promote_clean_base_snapshot(slot.path(), &snaps_dir, &base_sha, &["a/.git/b"]).is_err()
    );

    #[cfg(unix)]
    {
        let outside = TempDir::new().unwrap();
        let outside_sub = outside.path().join("b");
        fs::create_dir_all(&outside_sub).unwrap();
        fs::write(outside_sub.join("secret.txt"), b"outside").unwrap();
        std::os::unix::fs::symlink(outside.path(), slot.path().join("a")).unwrap();

        promote_clean_base_snapshot(slot.path(), &snaps_dir, &base_sha, &["a/b"]).unwrap();
        assert!(
            !snaps_dir.join(&base_sha).exists(),
            "intermediate symlink path must never be promoted"
        );

        fs::create_dir_all(slot.path().join("target")).unwrap();
        fs::write(slot.path().join("target/ok.bin"), b"safe").unwrap();
        promote_clean_base_snapshot(slot.path(), &snaps_dir, &base_sha, &["a/b", "target"])
            .unwrap();
        assert!(!snaps_dir.join(&base_sha).join("a").exists());
        assert_eq!(
            fs::read(snaps_dir.join(&base_sha).join("target/ok.bin")).unwrap(),
            b"safe"
        );
    }
}

#[test]
fn gc_evicts_expired_slot0_and_prunes_git_worktree_metadata() {
    let fx = RepoFixture::new();
    let worktrees = TempDir::new().unwrap();
    let base_sha = git(fx.work.path(), &["rev-parse", "HEAD"]);

    let slot = fx.acquire_merge_slot(&base_sha, "gc-prune");
    let slot_path = slot.path().to_path_buf();
    let count_worktrees = || {
        git(fx.work.path(), &["worktree", "list", "--porcelain"])
            .lines()
            .filter(|l| l.starts_with("worktree "))
            .count()
    };
    assert_eq!(count_worktrees(), 2);
    drop(slot);

    let opts = GcOptions {
        older_than_days: 0,
        ..GcOptions::default()
    };
    let report = run_gc(fx.data.path(), worktrees.path(), &opts).unwrap();
    assert!(!slot_path.exists());
    assert!(report.removed_files.iter().any(|p| p.ends_with("slot-0")));
    assert_eq!(
        count_worktrees(),
        1,
        "sweep_expired must run `git worktree prune` after evicting slot-0"
    );
}

#[test]
fn merge_pr_blocks_orphan_unrelated_histories_via_in_memory_merge_tree() {
    let fx = RepoFixture::new();
    git(fx.work.path(), &["checkout", "--orphan", "feat/orphan"]);
    git(fx.work.path(), &["rm", "-rf", "."]);
    fs::write(fx.work.path().join("orphan.txt"), "unrelated\n").unwrap();
    git(fx.work.path(), &["add", "orphan.txt"]);
    git(fx.work.path(), &["commit", "-m", "orphan root"]);
    let orphan_head = git(fx.work.path(), &["rev-parse", "HEAD"]);
    git(fx.work.path(), &["push", "-u", "origin", "feat/orphan"]);
    git(fx.work.path(), &["checkout", "main"]);

    let gh = MockGh {
        pr: pr_info(301, "feat/orphan", &orphan_head),
        merge_after_first_view: false,
        pending_first_check: false,
        views: AtomicUsize::new(0),
        checks: AtomicUsize::new(0),
        merged: AtomicUsize::new(0),
    };
    let noop = |_: &aaf_engine::throttle::Progress<'_>| {};
    let out = merge_pr(&fx.req(301, &orphan_head, 0), &gh, &FixedClock(1), &noop).unwrap();
    match out {
        PrMergeOutcome::Blocked { code, reason, .. } => {
            assert_eq!(code, "merge_conflict");
            assert!(reason.contains("unrelated histories"), "{reason}");
        }
        other => panic!("expected Blocked(merge_conflict), got {other:?}"),
    }
    assert_eq!(gh.merged.load(Ordering::SeqCst), 0);
    assert_eq!(fx.runs(), 0);
    assert!(!warm_slots_root(fx.data.path()).exists());
}

#[test]
fn merge_pr_refreshes_pr_state_after_waiting_for_pending_ci() {
    let fx = RepoFixture::new();
    git(fx.work.path(), &["checkout", "-b", "feat/ci-wait"]);
    fs::write(
        fx.work.path().join("src/lib.rs"),
        "pub fn v() -> i32 { 2 }\n",
    )
    .unwrap();
    git(fx.work.path(), &["add", "-A"]);
    git(fx.work.path(), &["commit", "-m", "feat"]);
    let pr_head = git(fx.work.path(), &["rev-parse", "HEAD"]);
    git(fx.work.path(), &["push", "-u", "origin", "feat/ci-wait"]);
    git(fx.work.path(), &["checkout", "main"]);

    let gh = MockGh {
        pr: pr_info(302, "feat/ci-wait", &pr_head),
        merge_after_first_view: true,
        pending_first_check: true,
        views: AtomicUsize::new(0),
        checks: AtomicUsize::new(0),
        merged: AtomicUsize::new(0),
    };
    let noop = |_: &aaf_engine::throttle::Progress<'_>| {};
    let out = merge_pr(&fx.req(302, &pr_head, 1), &gh, &FixedClock(2), &noop).unwrap();
    assert!(
        matches!(out, PrMergeOutcome::AlreadyMerged { pr_number: 302, .. }),
        "expected AlreadyMerged after ci_waited refresh, got {out:?}"
    );
    assert_eq!(gh.views.load(Ordering::SeqCst), 2);
    assert_eq!(gh.merged.load(Ordering::SeqCst), 0);
    assert_eq!(fx.runs(), 0);
}
