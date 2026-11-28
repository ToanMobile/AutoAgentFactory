#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Integration tests for serial per-repo merge locking in `aaf merge`.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aaf_domain::config::MachineConfig;
use aaf_domain::gate::GateMode;
use aaf_domain::ports::Clock;
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
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
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
        let cfg = format!(
            "[project]\nname = \"demo\"\nprofile = \"rust\"\n\n\
             [commands]\ntest_scoped = \"{test_scoped}\"\ntest_full = \"true\"\n\n\
             [gate]\nbase_ref = \"origin/main\"\ndocs_fast_path = true\n"
        );
        fs::write(work.path().join(".autofactory/config.toml"), cfg).unwrap();
        fs::write(work.path().join("src/lib.rs"), "pub fn v() -> i32 { 1 }\n").unwrap();
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
            require_checks: false,
            lock_timeout: Duration::from_secs(10),
            dry_run,
            machine: MachineConfig::default(),
            global_config: None,
            no_receipt_reuse: false,
            run_commands_despite_head_lock: false,
            strict_merged_head: false,
            keep_non_aaf_local: false,
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
        ..Default::default()
    }
}

struct StatefulGh {
    pr_number: u64,
    branch: String,
    head: String,
    merged: AtomicBool,
    merged_calls: AtomicUsize,
}

impl GhClient for StatefulGh {
    fn view_pr(&self, _repo_root: &Path, _pr_number: u64) -> Result<PrInfo, GitError> {
        let mut info = pr_info(self.pr_number, &self.branch, &self.head);
        if self.merged.load(Ordering::SeqCst) {
            info.state = "MERGED".to_string();
        }
        Ok(info)
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
        self.merged.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[test]
fn concurrent_duplicate_merge_for_same_pr_runs_gate_once_and_second_returns_already_merged() {
    let marker = TempDir::new().unwrap();
    let count_file = marker.path().join("runs.log");
    let cmd = format!("echo run >> '{}' && sleep 1", count_file.display());
    let fx = Fixture::new(&cmd);
    let head = fx.make_pr_branch("feat/dup-merge", |root| {
        fs::write(root.join("src/lib.rs"), "pub fn v() -> i32 { 77 }\n").unwrap();
    });

    let gh = Arc::new(StatefulGh {
        pr_number: 567,
        branch: "feat/dup-merge".to_string(),
        head: head.clone(),
        merged: AtomicBool::new(false),
        merged_calls: AtomicUsize::new(0),
    });

    let mut req1 = fx.req(567, &head, false);
    req1.machine.max_load_percent = 0;
    req1.lock_timeout = Duration::from_secs(30);
    let req2 = req1.clone();

    let gh1 = Arc::clone(&gh);
    let gh2 = Arc::clone(&gh);
    let t1 = std::thread::spawn(move || {
        let noop = |_: &aaf_engine::throttle::Progress<'_>| {};
        merge_pr(&req1, gh1.as_ref(), &FixedClock(50), &noop).unwrap()
    });
    std::thread::sleep(Duration::from_millis(150));
    let t2 = std::thread::spawn(move || {
        let noop = |_: &aaf_engine::throttle::Progress<'_>| {};
        merge_pr(&req2, gh2.as_ref(), &FixedClock(51), &noop).unwrap()
    });

    let o1 = t1.join().unwrap();
    let o2 = t2.join().unwrap();
    let outcomes = [o1, o2];
    let merged_n = outcomes
        .iter()
        .filter(|o| matches!(o, PrMergeOutcome::Merged { .. }))
        .count();
    let already_n = outcomes
        .iter()
        .filter(|o| matches!(o, PrMergeOutcome::AlreadyMerged { .. }))
        .count();
    assert_eq!((merged_n, already_n), (1, 1), "{outcomes:?}");
    assert_eq!(gh.merged_calls.load(Ordering::SeqCst), 1);

    let runs = fs::read_to_string(&count_file).unwrap();
    assert_eq!(
        runs.lines().count(),
        1,
        "test_scoped must execute only once across duplicate concurrent merges, got:\n{runs}"
    );
}

#[test]
fn contended_merge_lock_timeout_returns_blocked_merge_in_progress() {
    let fx = Fixture::new("true");
    let head = fx.make_pr_branch("feat/contended-lock", |root| {
        fs::write(root.join("src/lib.rs"), "pub fn v() -> i32 { 88 }\n").unwrap();
    });
    let locks_dir = fx.data.path().join("locks");
    fs::create_dir_all(&locks_dir).unwrap();
    let common_id = aaf_engine::gate::stats_repo_id(fx.work.path());
    let lock_file = fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(locks_dir.join(format!("{common_id}.merge.lock")))
        .unwrap();
    lock_file.try_lock().unwrap();

    let gh = StatefulGh {
        pr_number: 570,
        branch: "feat/contended-lock".to_string(),
        head: head.clone(),
        merged: AtomicBool::new(false),
        merged_calls: AtomicUsize::new(0),
    };
    let mut req = fx.req(570, &head, false);
    req.machine.max_load_percent = 0;
    req.lock_timeout = Duration::from_millis(120);
    let waited_events = Mutex::new(Vec::new());
    let progress = |ev: &aaf_engine::throttle::Progress<'_>| {
        waited_events.lock().unwrap().push(format!("{ev:?}"));
    };

    let out = merge_pr(&req, &gh, &FixedClock(60), &progress).unwrap();
    match out {
        PrMergeOutcome::Blocked { code, .. } => assert_eq!(code, "merge_in_progress"),
        other => panic!("expected Blocked(merge_in_progress), got {other:?}"),
    }
    assert!(
        waited_events
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.contains("WaitingForMergeLock")),
        "expected WaitingForMergeLock progress event"
    );
    drop(lock_file);
}
