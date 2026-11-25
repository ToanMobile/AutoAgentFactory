#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Integration tests for Phase A wiring (`tighten_trusted_base_from_env`,
//! `allow_common_receipt_write`), Phase B3 (`in_memory_merge_tree`,
//! parallel post-gate refresh + fetch skip), and Phase C (`WarmSlot` + GC).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use aaf_domain::config::{
    AuditScope, GlobalConfig, GlobalCrossAuditConfig, MachineConfig, RepoConfig,
};
use aaf_domain::gate::GateMode;
use aaf_domain::ports::Clock;
use aaf_engine::gate::{config_hash, stats_repo_id};
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
        fs::write(work.path().join(".gitignore"), "target/\n*.tmp\n").unwrap();
        fs::write(work.path().join("src/lib.rs"), "pub fn v() -> i32 { 1 }\n").unwrap();
        fs::write(
            work.path().join("src/other.rs"),
            "pub fn o() -> i32 { 1 }\n",
        )
        .unwrap();
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

    fn make_branch(&self, branch: &str, edit: impl FnOnce(&Path)) -> String {
        git(self.work.path(), &["checkout", "-b", branch, "origin/main"]);
        edit(self.work.path());
        git(self.work.path(), &["add", "-A"]);
        git(self.work.path(), &["commit", "-m", branch]);
        let sha = git(self.work.path(), &["rev-parse", "HEAD"]);
        git(self.work.path(), &["push", "-u", "origin", branch]);
        git(self.work.path(), &["checkout", "main"]);
        sha
    }

    fn advance_main(&self, edit: impl FnOnce(&Path), msg: &str) -> String {
        git(self.work.path(), &["checkout", "main"]);
        edit(self.work.path());
        git(self.work.path(), &["add", "-A"]);
        git(self.work.path(), &["commit", "-m", msg]);
        let sha = git(self.work.path(), &["rev-parse", "HEAD"]);
        git(self.work.path(), &["push", "origin", "main"]);
        sha
    }

    fn runs(&self) -> usize {
        fs::read_to_string(&self.counter_file)
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    fn req(
        &self,
        pr_number: u64,
        expect_head: &str,
        dry_run: bool,
        global_config: Option<GlobalConfig>,
    ) -> PrMergeRequest {
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
            require_checks: false,
            lock_timeout: Duration::from_secs(10),
            dry_run,
            machine: MachineConfig::default(),
            global_config,
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

struct SimpleGh {
    pr: PrInfo,
    merged: AtomicUsize,
}

impl GhClient for SimpleGh {
    fn view_pr(&self, _: &Path, _: u64) -> Result<PrInfo, GitError> {
        Ok(self.pr.clone())
    }
    fn pr_checks(&self, _: &Path, _: u64) -> Result<Vec<PrCheck>, GitError> {
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

struct FetchSkipObserverGh {
    pr: PrInfo,
    views: AtomicUsize,
    removed_fetch_head_on_refresh: AtomicBool,
    fetch_head_absent_at_merge: AtomicBool,
    merged: AtomicUsize,
}

impl GhClient for FetchSkipObserverGh {
    fn view_pr(&self, repo_root: &Path, _: u64) -> Result<PrInfo, GitError> {
        if self.views.fetch_add(1, Ordering::SeqCst) == 1 {
            let fetch_head = repo_root.join(".git/FETCH_HEAD");
            if fetch_head.exists() && fs::remove_file(&fetch_head).is_ok() {
                self.removed_fetch_head_on_refresh
                    .store(true, Ordering::SeqCst);
            }
        }
        Ok(self.pr.clone())
    }
    fn pr_checks(&self, _: &Path, _: u64) -> Result<Vec<PrCheck>, GitError> {
        Ok(vec![])
    }
    fn merge_pr(
        &self,
        repo_root: &Path,
        _: u64,
        _: &str,
        _: MergeStrategy,
        _: Option<&str>,
        _: Option<&Path>,
    ) -> Result<(), GitError> {
        if !repo_root.join(".git/FETCH_HEAD").exists() {
            self.fetch_head_absent_at_merge
                .store(true, Ordering::SeqCst);
        }
        self.merged.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[test]
fn warm_slot_reuses_fixed_path_falls_back_under_contention_and_wipes_on_drop() {
    let fx = RepoFixture::new();
    let base_sha = git(fx.work.path(), &["rev-parse", "HEAD"]);
    let common_id = stats_repo_id(fx.work.path());
    let acq = |tag| {
        WarmSlot::acquire(
            fx.work.path(),
            fx.data.path(),
            &common_id,
            SlotKind::Merge,
            &base_sha,
            tag,
        )
        .unwrap()
    };

    let slot1 = acq("pr1");
    assert!(slot1.is_persistent());
    let fixed_path = slot1.path().to_path_buf();
    let lock_path = fixed_path.parent().unwrap().join("slot-0.lock");
    assert!(lock_path.is_file());

    fs::create_dir_all(slot1.path().join("target")).unwrap();
    fs::write(slot1.path().join("target/pr_artifact.o"), b"untrusted").unwrap();
    fs::write(slot1.path().join("untracked_pr.txt"), b"dirty").unwrap();
    fs::write(slot1.path().join("ignored.tmp"), b"ignored").unwrap();

    // Concurrent acquisition falls back to ephemeral; dropping ephemeral removes it.
    let slot2 = acq("pr2");
    assert!(!slot2.is_persistent());
    let eph_path = slot2.path().to_path_buf();
    assert_ne!(eph_path, fixed_path);
    assert!(eph_path.is_dir());
    drop(slot2);
    assert!(!eph_path.exists());
    assert!(slot1.path().join("target/pr_artifact.o").exists());

    // Dropping persistent `slot-0` wipes PR artifacts and keeps `slot-0.lock`.
    drop(slot1);
    assert!(lock_path.is_file(), "slot-0.lock must NEVER be unlinked");
    assert!(!fixed_path.join("target/pr_artifact.o").exists());
    assert!(!fixed_path.join("untracked_pr.txt").exists());
    assert!(!fixed_path.join("ignored.tmp").exists());

    let slot3 = acq("pr3");
    assert!(slot3.is_persistent());
    assert_eq!(slot3.path(), fixed_path.as_path());
}

#[test]
fn warm_slot_cow_snapshot_restores_ignored_artifacts_and_rejects_symlinks() {
    let fx = RepoFixture::new();
    let base_sha = git(fx.work.path(), &["rev-parse", "HEAD"]);
    let common_id = stats_repo_id(fx.work.path());
    let snaps_dir = warm_slots_root(fx.data.path())
        .join("merge")
        .join(&common_id)
        .join("base-snaps");
    let acq = |tag| {
        WarmSlot::acquire(
            fx.work.path(),
            fx.data.path(),
            &common_id,
            SlotKind::Merge,
            &base_sha,
            tag,
        )
        .unwrap()
    };

    {
        let slot = acq("seed");
        fs::create_dir_all(slot.path().join("target")).unwrap();
        fs::write(slot.path().join("target/warm.bin"), b"base-warm-cache").unwrap();
        promote_clean_base_snapshot(slot.path(), &snaps_dir, &base_sha, &["target"]).unwrap();
    }
    // Forged tracked path in snapshot must never overwrite tracked `src/lib.rs`.
    let snap_dir = snaps_dir.join(&base_sha);
    fs::create_dir_all(snap_dir.join("src")).unwrap();
    fs::write(snap_dir.join("src/lib.rs"), "pub fn v() -> i32 { 999 }\n").unwrap();

    {
        let slot = acq("warm");
        assert_eq!(
            fs::read(slot.path().join("target/warm.bin")).unwrap(),
            b"base-warm-cache"
        );
        assert_eq!(
            fs::read_to_string(slot.path().join("src/lib.rs")).unwrap(),
            "pub fn v() -> i32 { 1 }\n"
        );
    }

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("/etc/hosts", snap_dir.join("target/escape")).unwrap();
        let slot = acq("symlink-reject");
        assert!(!slot.path().join("target/warm.bin").exists());
        assert!(!slot.path().join("target/escape").exists());
    }
}

#[test]
fn non_ff_merge_conflict_blocks_via_in_memory_merge_tree_without_worktree() {
    let fx = RepoFixture::new();
    let pr_head = fx.make_branch("feat/conflict", |root| {
        fs::write(root.join("src/lib.rs"), "pub fn v() -> i32 { 10 }\n").unwrap();
    });
    fx.advance_main(
        |root| fs::write(root.join("src/lib.rs"), "pub fn v() -> i32 { 20 }\n").unwrap(),
        "conflicting base commit",
    );
    let gh = SimpleGh {
        pr: pr_info(201, "feat/conflict", &pr_head),
        merged: AtomicUsize::new(0),
    };
    let noop = |_: &aaf_engine::throttle::Progress<'_>| {};
    let out = merge_pr(
        &fx.req(201, &pr_head, false, None),
        &gh,
        &FixedClock(1),
        &noop,
    )
    .unwrap();
    match out {
        PrMergeOutcome::Blocked { code, reason, .. } => {
            assert_eq!(code, "merge_conflict");
            assert!(reason.contains("src/lib.rs"), "{reason}");
        }
        other => panic!("expected Blocked(merge_conflict), got {other:?}"),
    }
    assert_eq!(gh.merged.load(Ordering::SeqCst), 0);
    assert_eq!(fx.runs(), 0);
    assert!(!warm_slots_root(fx.data.path()).exists());
}

#[test]
fn non_ff_dry_run_hits_common_receipt_on_real_merge_and_skips_second_fetch() {
    let fx = RepoFixture::new();
    let pr_head = fx.make_branch("feat/non-ff", |root| {
        fs::write(root.join("src/lib.rs"), "pub fn v() -> i32 { 42 }\n").unwrap();
    });
    fx.advance_main(
        |root| fs::write(root.join("src/other.rs"), "pub fn o() -> i32 { 99 }\n").unwrap(),
        "disjoint base update",
    );
    let noop = |_: &aaf_engine::throttle::Progress<'_>| {};
    let gh_dry = SimpleGh {
        pr: pr_info(202, "feat/non-ff", &pr_head),
        merged: AtomicUsize::new(0),
    };
    let dry = merge_pr(
        &fx.req(202, &pr_head, true, None),
        &gh_dry,
        &FixedClock(10),
        &noop,
    )
    .unwrap();
    assert!(matches!(dry, PrMergeOutcome::DryRunClear { .. }), "{dry:?}");
    assert_eq!(fx.runs(), 1);

    // Wipe warm-slots to prove real merge hits common_receipt via in_memory_merge_tree.
    let slots_root = warm_slots_root(fx.data.path());
    if slots_root.exists() {
        fs::remove_dir_all(&slots_root).unwrap();
    }

    let gh_real = FetchSkipObserverGh {
        pr: pr_info(202, "feat/non-ff", &pr_head),
        views: AtomicUsize::new(0),
        removed_fetch_head_on_refresh: AtomicBool::new(false),
        fetch_head_absent_at_merge: AtomicBool::new(false),
        merged: AtomicUsize::new(0),
    };
    let merged = merge_pr(
        &fx.req(202, &pr_head, false, None),
        &gh_real,
        &FixedClock(11),
        &noop,
    )
    .unwrap();
    assert!(
        matches!(merged, PrMergeOutcome::Merged { pr_number: 202, .. }),
        "{merged:?}"
    );
    assert_eq!(fx.runs(), 1);
    assert!(!slots_root.exists());
    assert!(gh_real.removed_fetch_head_on_refresh.load(Ordering::SeqCst));
    assert!(gh_real.fetch_head_absent_at_merge.load(Ordering::SeqCst));
    assert_eq!(gh_real.merged.load(Ordering::SeqCst), 1);
}

#[test]
fn pr_merge_enforces_tier1_global_and_tier3_local_tightening() {
    let fx = RepoFixture::new();
    let pr_head = fx.make_branch("feat/tighten", |root| {
        fs::write(root.join("src/lib.rs"), "pub fn v() -> i32 { 7 }\n").unwrap();
    });
    let noop = |_: &aaf_engine::throttle::Progress<'_>| {};
    let gh = SimpleGh {
        pr: pr_info(203, "feat/tighten", &pr_head),
        merged: AtomicUsize::new(0),
    };

    let base_hash = match merge_pr(
        &fx.req(203, &pr_head, true, None),
        &gh,
        &FixedClock(20),
        &noop,
    )
    .unwrap()
    {
        PrMergeOutcome::DryRunClear { receipt, .. } => receipt.config_hash,
        other => panic!("expected DryRunClear, got {other:?}"),
    };
    assert_eq!(fx.runs(), 1);

    let strict_global = GlobalConfig {
        cross_audit: GlobalCrossAuditConfig {
            score_gate: Some(true),
            audit_scope: Some(AuditScope::Always),
            min_average_score: Some(98),
            ..GlobalCrossAuditConfig::default()
        },
        ..GlobalConfig::default()
    };
    let tight_hash = match merge_pr(
        &fx.req(203, &pr_head, true, Some(strict_global)),
        &gh,
        &FixedClock(21),
        &noop,
    )
    .unwrap()
    {
        PrMergeOutcome::DryRunClear { receipt, .. } => receipt.config_hash,
        other => panic!("expected DryRunClear under strict global, got {other:?}"),
    };
    assert_ne!(tight_hash, base_hash);
    assert_eq!(fx.runs(), 2);

    let mut expected_cfg: RepoConfig = toml::from_str(
        &fs::read_to_string(fx.work.path().join(".autofactory/config.toml")).unwrap(),
    )
    .unwrap();
    expected_cfg.cross_audit.score_gate = true;
    expected_cfg.cross_audit.audit_scope = AuditScope::Always;
    expected_cfg.cross_audit.min_average_score = 98;
    assert_eq!(tight_hash, config_hash(&expected_cfg));

    // Tier-3 local config tightening (`head_lock_globs`) blocks unapproved PR.
    fs::write(
        fx.work.path().join(".autofactory/config.local.toml"),
        "[gate]\nhead_lock_globs = [\"src/lib.rs\"]\n",
    )
    .unwrap();
    let out_blocked = merge_pr(
        &fx.req(203, &pr_head, false, None),
        &gh,
        &FixedClock(22),
        &noop,
    )
    .unwrap();
    match out_blocked {
        PrMergeOutcome::Blocked { code, .. } => assert_eq!(code, "cross_approval_required"),
        other => panic!("expected Blocked(cross_approval_required), got {other:?}"),
    }
    assert_eq!(gh.merged.load(Ordering::SeqCst), 0);
}

#[test]
fn gc_preserves_live_locked_warm_slot_and_evicts_expired_unlocked() {
    let fx = RepoFixture::new();
    let worktrees = TempDir::new().unwrap();
    let base_sha = git(fx.work.path(), &["rev-parse", "HEAD"]);
    let common_id = stats_repo_id(fx.work.path());
    let slot_parent = warm_slots_root(fx.data.path())
        .join("merge")
        .join(&common_id);
    let snaps_dir = slot_parent.join("base-snaps");
    let acq = |tag| {
        WarmSlot::acquire(
            fx.work.path(),
            fx.data.path(),
            &common_id,
            SlotKind::Merge,
            &base_sha,
            tag,
        )
        .unwrap()
    };

    let slot1 = acq("live1");
    fs::create_dir_all(slot1.path().join("target")).unwrap();
    fs::write(slot1.path().join("target/cache.bin"), b"ok").unwrap();
    promote_clean_base_snapshot(slot1.path(), &snaps_dir, &base_sha, &["target"]).unwrap();

    let slot2 = acq("live2");
    let stale_eph = slot_parent.join("ephemeral-stale-999-0");
    fs::create_dir_all(&stale_eph).unwrap();
    fs::write(stale_eph.join("junk.txt"), b"stale").unwrap();

    let opts = GcOptions {
        older_than_days: 0,
        ..GcOptions::default()
    };
    let report1 = run_gc(fx.data.path(), worktrees.path(), &opts).unwrap();
    assert!(
        slot1.path().exists(),
        "live-locked slot-0 must be preserved"
    );
    assert!(snaps_dir.join(&base_sha).exists());
    assert!(
        slot2.path().exists(),
        "live-locked ephemeral must be preserved"
    );
    assert!(
        !stale_eph.exists(),
        "unlocked expired ephemeral must be evicted"
    );
    assert!(
        report1
            .removed_files
            .iter()
            .any(|p| p.contains("ephemeral-stale-999-0"))
    );

    let slot0_path = slot1.path().to_path_buf();
    let slot0_lock = slot_parent.join("slot-0.lock");
    drop(slot2);
    drop(slot1);

    let report2 = run_gc(fx.data.path(), worktrees.path(), &opts).unwrap();
    assert!(!snaps_dir.join(&base_sha).exists());
    assert!(!slot0_path.exists());
    assert!(slot0_lock.is_file(), "GC must NEVER unlink slot-0.lock");
    assert!(report2.removed_files.iter().any(|p| p.contains(&base_sha)));
}
