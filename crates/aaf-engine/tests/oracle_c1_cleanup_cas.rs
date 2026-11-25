#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Oracle tests for Gói C1: atomic CAS `Review -> Done`, `RealGit::cleanup` /
//! `worktree::remove` safety, `cleanup_verified_merged`, `sweep_squash_merged`,
//! and `verify_squash_merges` `--limit 100` saturation fallback.

#[path = "git_support.rs"]
mod support;

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use aaf_domain::account::AccountProfile;
use aaf_domain::blocked::BlockedReason;
use aaf_domain::budget::RootBudget;
use aaf_domain::config::RepoConfig;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::gate::{
    GateMode, GateStatus, HeadLockReport, LockStatus, RECEIPT_SCHEMA_VERSION, Receipt,
    StoreComplianceReport,
};
use aaf_domain::ports::{
    AgentOutcome, AgentRequest, AgentRunner, BudgetStore, Clock, ClosedFingerprint, EventSink,
    FingerprintStore, KvStore, LimitHit, META_CONSECUTIVE_ROOT_FAILS, PortError, PortResult,
    RunStore, TaskStore,
};
use aaf_domain::run::{RunFinish, RunRecord};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::{Db, complete_review_task};
use aaf_engine::factory::{
    AccountPool, Deps, GateRunner, GitOps, Grant, Hooks, Integration, Mode, Notifier, RealGit,
    Settings, Store, resolve_base, run,
};
use aaf_engine::git::{GitError, MergeOutcome, PrOutcome, TaskWorktree};
use aaf_engine::process::{CommandOutput, CommandRunner};
use aaf_engine::webhook::WebhookEvent;
use serde_json::json;
use support::{Fixture, commit_file, git};

struct FixedClock(i64);
impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0
    }
}

struct NullDeps;
impl Notifier for NullDeps {
    fn notify(&self, _: &WebhookEvent) -> Result<bool, String> {
        Ok(false)
    }
}
impl AccountPool for NullDeps {
    fn acquire(&self) -> PortResult<Option<Grant>> {
        Ok(Some(Grant::new(None, None)))
    }
    fn report_limit(&self, _: &str, _: &LimitHit) -> PortResult<()> {
        Ok(())
    }
    fn profiles(&self) -> PortResult<Vec<AccountProfile>> {
        Ok(Vec::new())
    }
}
impl AgentRunner for NullDeps {
    fn run(&self, _: &AgentRequest) -> PortResult<AgentOutcome> {
        Err(PortError::Invalid("unexpected agent run".into()))
    }
}
impl GateRunner for NullDeps {
    fn run(&self, tree: &Path) -> PortResult<Receipt> {
        Ok(Receipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            repo_root: tree.display().to_string(),
            tree_fp: "fp".into(),
            head_sha: None,
            base_sha: None,
            mode: GateMode::Scoped,
            status: GateStatus::Clear,
            reasons: Vec::new(),
            config_hash: "cfg".into(),
            commands: Vec::new(),
            static_findings: Vec::new(),
            suppressions: 0,
            head_lock: HeadLockReport {
                status: LockStatus::Clear,
                lock_fp: String::new(),
                locked_changes: Vec::new(),
                approval: None,
            },
            store_compliance: StoreComplianceReport::not_applicable(),
            created_at: 0,
        })
    }
    fn head_lock(&self, _: &Path) -> PortResult<HeadLockReport> {
        Ok(HeadLockReport {
            status: LockStatus::Clear,
            lock_fp: String::new(),
            locked_changes: Vec::new(),
            approval: None,
        })
    }
}

macro_rules! fwd {
    ($(fn $name:ident(&self $(, $a:ident: $t:ty)*) -> $ret:ty;)*) => {
        $(fn $name(&self $(, $a: $t)*) -> $ret { self.inner.$name($($a),*) })*
    };
}
macro_rules! git_const {
    ($(fn $name:ident(&self $(, _: $t:ty)*) -> $ret:ty => $body:expr;)*) => {
        $(fn $name(&self $(, _: $t)*) -> $ret { $body })*
    };
}

/// Flips `race_task_id` from `Review -> Gating` inside the first `get_task`
/// observing `Review`, returning the pre-flip `Review` snapshot to simulate
/// a concurrent `aaf board rework`.
struct RacingStore {
    inner: Arc<Db>,
    race_task_id: String,
    flipped: AtomicBool,
}
impl TaskStore for RacingStore {
    fn get_task(&self, id: &str) -> PortResult<Task> {
        let task = self.inner.get_task(id)?;
        if id == self.race_task_id
            && task.state == TaskState::Review
            && !self.flipped.swap(true, Ordering::SeqCst)
        {
            self.inner
                .set_state(id, TaskState::Gating, None, task.updated_at + 1)?;
        }
        Ok(task)
    }
    fwd! {
        fn insert_task(&self, task: &Task) -> PortResult<()>;
        fn list_tasks(&self, repo: Option<&str>, state: Option<TaskState>) -> PortResult<Vec<Task>>;
        fn set_state(&self, id: &str, s: TaskState, b: Option<&BlockedReason>, now: i64) -> PortResult<()>;
        fn append_context_note(&self, id: &str, note: &str, now: i64) -> PortResult<()>;
        fn increment_unblock_cycles(&self, id: &str, now: i64) -> PortResult<u32>;
        fn add_dep(&self, task_id: &str, blocked_by: &str) -> PortResult<()>;
        fn remove_dep(&self, task_id: &str, blocked_by: &str) -> PortResult<bool>;
        fn deps_of(&self, task_id: &str) -> PortResult<Vec<String>>;
        fn all_deps(&self) -> PortResult<Vec<(String, String)>>;
        fn children_of(&self, parent_id: &str) -> PortResult<Vec<Task>>;
    }
}
impl BudgetStore for RacingStore {
    fwd! {
        fn put_budget(&self, b: &RootBudget) -> PortResult<()>;
        fn get_budget(&self, id: &str) -> PortResult<RootBudget>;
        fn add_usage(&self, id: &str, tok: u64, secs: u64, cyc: u32) -> PortResult<RootBudget>;
        fn set_frozen(&self, id: &str, frozen: bool) -> PortResult<()>;
    }
}
impl RunStore for RacingStore {
    fwd! {
        fn insert_run(&self, run: &RunRecord) -> PortResult<()>;
        fn finish_run(&self, id: &str, finish: &RunFinish) -> PortResult<()>;
        fn runs_for_task(&self, task_id: &str) -> PortResult<Vec<RunRecord>>;
        fn tokens_since(&self, account_id: &str, since: i64) -> PortResult<u64>;
    }
}
impl EventSink for RacingStore {
    fwd! {
        fn emit(&self, ev: &FactoryEvent) -> PortResult<i64>;
        fn events_for_task(&self, id: &str, limit: u32) -> PortResult<Vec<FactoryEvent>>;
    }
}
impl KvStore for RacingStore {
    fwd! {
        fn get_meta(&self, key: &str) -> PortResult<Option<String>>;
        fn set_meta(&self, key: &str, value: &str) -> PortResult<()>;
    }
}
impl FingerprintStore for RacingStore {
    fwd! {
        fn record_closed(&self, closed: &ClosedFingerprint) -> PortResult<()>;
        fn find_closed_since(&self, fp: &str, repo: &str, since: i64) -> PortResult<Option<ClosedFingerprint>>;
        fn find_open_task(&self, fp: &str, repo: &str) -> PortResult<Option<String>>;
    }
}
impl Store for RacingStore {}

struct SquashFakeGit {
    wt_root: PathBuf,
    head_oid: String,
    dirty_on_remove: bool,
    remove_called: AtomicBool,
}
impl GitOps for SquashFakeGit {
    fn task_worktree(&self, id: &str) -> PortResult<TaskWorktree> {
        Ok(TaskWorktree {
            path: self.wt_root.join(id),
            branch: format!("aaf/{id}"),
        })
    }
    fn base_sha(&self) -> PortResult<String> {
        Ok(self.head_oid.clone())
    }
    git_const! {
        fn commit_all(&self, _: &TaskWorktree, _: &str) -> PortResult<()> => Ok(());
        fn has_changes(&self, _: &TaskWorktree) -> PortResult<bool> => Ok(false);
        fn diff(&self, _: &TaskWorktree) -> PortResult<String> => Ok(String::new());
        fn integrate(&self, _: &[String]) -> PortResult<Integration> => Err(PortError::Invalid("unused".into()));
        fn discard(&self, _: &Integration) -> PortResult<()> => Ok(());
        fn rebase(&self, _: &TaskWorktree) -> PortResult<Vec<String>> => Ok(Vec::new());
        fn merge(&self, _: &TaskWorktree, _: &dyn Fn(&Path) -> Result<bool, String>) -> PortResult<MergeOutcome> => Err(PortError::Invalid("unused".into()));
        fn open_pr(&self, _: &TaskWorktree, _: &str, _: &Receipt, _: Option<&str>) -> PortResult<PrOutcome> => Err(PortError::Invalid("unused".into()));
        fn fetch_base(&self) -> PortResult<()> => Ok(());
        fn is_merged(&self, _: &str) -> PortResult<bool> => Ok(false);
        fn cleanup(&self, _: &str) -> PortResult<()> => Ok(());
    }
    fn existing_worktree_head(&self, id: &str) -> Result<Option<(TaskWorktree, String)>, GitError> {
        Ok(Some((
            TaskWorktree {
                path: self.wt_root.join(id),
                branch: format!("aaf/{id}"),
            },
            self.head_oid.clone(),
        )))
    }
    fn verified_squash_merges(
        &self,
        candidates: &[(String, String)],
    ) -> Result<HashMap<String, String>, GitError> {
        Ok(candidates.iter().cloned().collect())
    }
    fn remove_verified_merged(&self, wt: &TaskWorktree, _: &str) -> Result<(), GitError> {
        self.remove_called.store(true, Ordering::SeqCst);
        if self.dirty_on_remove {
            return Err(GitError::Dirty(wt.path.clone()));
        }
        Ok(())
    }
}

fn make_review_task(repo: &Path, id: &str, fp: Option<&str>) -> Task {
    let mut t = Task::new_root(
        id,
        repo.to_string_lossy(),
        format!("Task {id}"),
        ImmutableSpec {
            description: "desc".into(),
            acceptance: vec!["ok".into()],
        },
        Priority::P2,
        1_700_000_000,
    );
    t.state = TaskState::Review;
    t.fingerprint = fp.map(str::to_string);
    t
}

fn test_deps(repo: &Path, data_dir: &Path, store: Arc<dyn Store>, git: Arc<dyn GitOps>) -> Deps {
    Deps {
        store,
        clock: Arc::new(FixedClock(1_700_000_100)),
        runner: Arc::new(NullDeps),
        accounts: Arc::new(NullDeps),
        git,
        gate: Arc::new(NullDeps),
        notifier: Arc::new(NullDeps),
        settings: Settings {
            repo_root: repo.to_string_lossy().into_owned(),
            repo: RepoConfig::default(),
            max_parallel: 1,
            auto_merge: false,
            sandboxed: false,
            hook_required: false,
            agent_path: std::ffi::OsString::new(),
            mobile: false,
            data_dir: data_dir.to_path_buf(),
            home: data_dir.to_path_buf(),
            owner_github_allowlist: Vec::new(),
        },
    }
}

fn real_git(fx: &Fixture) -> RealGit {
    let base = resolve_base(&fx.repo, "auto").unwrap();
    RealGit::new(
        fx.repo.clone(),
        fx.worktrees.clone(),
        fx.locks.clone(),
        base,
    )
}

fn snapshots_of(repo: &Path, id: &str) -> String {
    let prefix = format!("refs/aaf/snapshots/{id}/");
    git(repo, &["for-each-ref", "--format=%(refname)", &prefix])
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    git(
        repo,
        &["branch", "--list", "--format=%(refname:short)", branch],
    ) == branch
}

#[test]
fn c1_1_cleanup_removes_merged_branch_and_snapshots_when_worktree_dir_missing() {
    let fx = Fixture::new();
    let real = real_git(&fx);
    let wt = real.task_worktree("t1").unwrap();
    let head = commit_file(&wt.path, "t1.txt", "ok\n", "t1 commit");
    git(&fx.repo, &["merge", "--ff-only", "aaf/t1"]);
    git(&fx.repo, &["update-ref", "refs/aaf/snapshots/t1/s1", &head]);

    fs::remove_dir_all(&wt.path).unwrap();
    real.cleanup("t1").unwrap();

    assert!(!branch_exists(&fx.repo, "aaf/t1"));
    assert_eq!(snapshots_of(&fx.repo, "t1"), "");
}

#[test]
fn c1_2_cleanup_refuses_unmerged_branch_with_self_tracking_remote_and_snapshot() {
    let fx = Fixture::with_remote();
    let real = real_git(&fx);
    let wt = real.task_worktree("t2").unwrap();
    let head = commit_file(&wt.path, "t2.txt", "unmerged\n", "t2 unmerged");
    git(&fx.repo, &["push", "-q", "origin", "aaf/t2"]);
    git(&fx.repo, &["update-ref", "refs/aaf/snapshots/t2/s1", &head]);

    let err = real.cleanup("t2").unwrap_err();
    assert!(
        err.to_string().contains("no other branch") || err.to_string().contains("aaf/t2"),
        "unexpected error: {err}"
    );
    assert!(wt.path.is_dir(), "worktree must remain intact");
    assert!(branch_exists(&fx.repo, "aaf/t2"));
    assert_eq!(snapshots_of(&fx.repo, "t2"), "refs/aaf/snapshots/t2/s1");
}

#[test]
fn c1_3_cleanup_verified_merged_handles_squash_clean_and_refuses_dirty_or_mismatch() {
    let fx = Fixture::new();
    let real = real_git(&fx);
    let wt = real.task_worktree("t3").unwrap();
    let head = commit_file(&wt.path, "t3.txt", "squash\n", "t3 feat");
    git(&fx.repo, &["update-ref", "refs/aaf/snapshots/t3/s1", &head]);
    commit_file(&fx.repo, "t3.txt", "squash\n", "squash PR #3");
    assert!(!real.is_merged("t3").unwrap());
    let wrong = "0".repeat(40);
    assert!(real.cleanup_verified_merged("t3", &wrong).is_err());
    assert!(wt.path.is_dir() && branch_exists(&fx.repo, "aaf/t3"));
    assert_eq!(snapshots_of(&fx.repo, "t3"), "refs/aaf/snapshots/t3/s1");
    fs::write(wt.path.join("wip.txt"), "keep\n").unwrap();
    assert!(real.cleanup_verified_merged("t3", &head).is_err());
    assert!(wt.path.join("wip.txt").is_file() && branch_exists(&fx.repo, "aaf/t3"));
    assert_eq!(snapshots_of(&fx.repo, "t3"), "refs/aaf/snapshots/t3/s1");
    fs::remove_file(wt.path.join("wip.txt")).unwrap();
    real.cleanup_verified_merged("t3", &head).unwrap();
    assert!(!wt.path.exists() && !branch_exists(&fx.repo, "aaf/t3"));
    assert_eq!(snapshots_of(&fx.repo, "t3"), "");
    let wt2 = real.task_worktree("t3m").unwrap();
    let h2 = commit_file(&wt2.path, "t3m.txt", "h2\n", "h2");
    git(&fx.repo, &["update-ref", "refs/aaf/snapshots/t3m/s1", &h2]);
    fs::remove_dir_all(&wt2.path).unwrap();
    assert!(real.cleanup_verified_merged("t3m", &wrong).is_err());
    assert!(branch_exists(&fx.repo, "aaf/t3m"));
    assert_eq!(snapshots_of(&fx.repo, "t3m"), "refs/aaf/snapshots/t3m/s1");
    real.cleanup_verified_merged("t3m", &h2).unwrap();
    assert!(!branch_exists(&fx.repo, "aaf/t3m") && snapshots_of(&fx.repo, "t3m").is_empty());
}

#[test]
fn c1_4_sweep_squash_merged_cas_protects_concurrent_review_to_gating_rework() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, data) = (tmp.path().join("repo"), tmp.path().join("data"));
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir_all(&data).unwrap();
    let db = Arc::new(Db::open(&data.join("aaf.db")).unwrap());
    db.insert_task(&make_review_task(&repo, "t-race", None))
        .unwrap();

    let racing = Arc::new(RacingStore {
        inner: Arc::clone(&db),
        race_task_id: "t-race".to_string(),
        flipped: AtomicBool::new(false),
    });
    let git = Arc::new(SquashFakeGit {
        wt_root: tmp.path().join("wt"),
        head_oid: "a".repeat(40),
        dirty_on_remove: false,
        remove_called: AtomicBool::new(false),
    });
    let deps = test_deps(&repo, &data, racing, Arc::clone(&git) as Arc<dyn GitOps>);
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };
    let summary = run(&deps, Mode::Once, &hooks).unwrap();

    assert_eq!(db.get_task("t-race").unwrap().state, TaskState::Gating);
    assert!(!git.remove_called.load(Ordering::SeqCst));
    assert!(!summary.done.contains(&"t-race".to_string()));
    assert!(
        db.events_for_task_by_kinds("t-race", &[EventKind::MergeResult], 10)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn c1_5_sweep_squash_merged_dirty_worktree_still_moves_done_and_emits_merge_result() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, data) = (tmp.path().join("repo"), tmp.path().join("data"));
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir_all(&data).unwrap();
    let db = Arc::new(Db::open(&data.join("aaf.db")).unwrap());
    db.set_meta(META_CONSECUTIVE_ROOT_FAILS, "2").unwrap();
    db.insert_task(&make_review_task(&repo, "t-dirty", Some("fp-dirty")))
        .unwrap();

    let git = Arc::new(SquashFakeGit {
        wt_root: tmp.path().join("wt"),
        head_oid: "b".repeat(40),
        dirty_on_remove: true,
        remove_called: AtomicBool::new(false),
    });
    let deps = test_deps(&repo, &data, Arc::clone(&db) as Arc<dyn Store>, git);
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };
    let summary = run(&deps, Mode::Once, &hooks).unwrap();

    assert_eq!(db.get_task("t-dirty").unwrap().state, TaskState::Done);
    assert!(summary.done.contains(&"t-dirty".to_string()));
    assert!(
        summary
            .errors
            .iter()
            .any(|e| e.contains("t-dirty") && e.contains("leftover")),
        "expected leftover warning in {:?}",
        summary.errors
    );
    let evs = db
        .events_for_task_by_kinds("t-dirty", &[EventKind::MergeResult], 10)
        .unwrap();
    assert_eq!(evs.len(), 1);
    let payload: serde_json::Value = serde_json::from_str(&evs[0].payload_json).unwrap();
    assert_eq!(payload["merged"], json!(true));
    assert_eq!(payload["kind"], json!("squash_pr"));
    assert!(
        db.find_closed_since("fp-dirty", &repo.to_string_lossy(), 0)
            .unwrap()
            .is_some()
    );
    assert_eq!(
        db.get_meta(META_CONSECUTIVE_ROOT_FAILS).unwrap().as_deref(),
        Some("0")
    );
}

struct SaturatedGh {
    bulk_rows: String,
    per_head_calls: AtomicUsize,
    rate_limit: bool,
    match_head: String,
    match_oid: String,
    merge_oid: String,
}
impl CommandRunner for SaturatedGh {
    fn run(&self, program: &str, args: &[&str], _: &Path) -> io::Result<CommandOutput> {
        assert_eq!(program, "gh");
        if let Some(pos) = args.iter().position(|a| *a == "--head") {
            let head = args[pos + 1];
            self.per_head_calls.fetch_add(1, Ordering::SeqCst);
            if self.rate_limit {
                return Ok(CommandOutput {
                    exit_code: Some(1),
                    stdout: String::new(),
                    stderr: "HTTP 403: API rate limit exceeded".into(),
                });
            }
            let stdout = if head == self.match_head {
                json!([{
                    "headRefName": head, "headRefOid": self.match_oid,
                    "baseRefName": "main", "mergeCommit": { "oid": self.merge_oid },
                    "isCrossRepository": false
                }])
                .to_string()
            } else {
                "[]".into()
            };
            return Ok(CommandOutput {
                exit_code: Some(0),
                stdout,
                stderr: String::new(),
            });
        }
        Ok(CommandOutput {
            exit_code: Some(0),
            stdout: self.bulk_rows.clone(),
            stderr: String::new(),
        })
    }
}

#[test]
fn c1_6_verify_squash_merges_fallback_only_when_saturated_and_stops_on_rate_limit() {
    let fx = Fixture::with_remote();
    let real = real_git(&fx);
    let wt = real.task_worktree("t_sat").unwrap();
    let local_head = commit_file(&wt.path, "sat.txt", "sat\n", "sat commit");
    let clone = fx.repo.parent().unwrap().join("sat_clone");
    let bare = fx.remote.as_ref().unwrap();
    assert!(
        Command::new("git")
            .args([
                "clone",
                "-q",
                bare.to_str().unwrap(),
                clone.to_str().unwrap()
            ])
            .status()
            .unwrap()
            .success()
    );
    for (k, v) in [
        ("user.name", "s"),
        ("user.email", "s@e.invalid"),
        ("commit.gpgsign", "false"),
        ("core.hooksPath", "/dev/null"),
    ] {
        git(&clone, &["config", k, v]);
    }
    let squash_oid = commit_file(&clone, "sat.txt", "sat\n", "squash sat");
    git(&clone, &["push", "-q", "origin", "main"]);

    let filler: Vec<_> = (0..100)
        .map(|i| {
            json!({
                "headRefName": format!("other/pr-{i}"), "headRefOid": "1".repeat(40),
                "baseRefName": "main", "mergeCommit": { "oid": "2".repeat(40) },
                "isCrossRepository": false
            })
        })
        .collect();
    let bulk_rows = serde_json::Value::Array(filler).to_string();
    let candidates = vec![
        ("t_sat".to_string(), local_head.clone()),
        ("t_second".to_string(), "3".repeat(40)),
    ];

    let gh_ok = SaturatedGh {
        bulk_rows: bulk_rows.clone(),
        per_head_calls: AtomicUsize::new(0),
        rate_limit: false,
        match_head: "aaf/t_sat".into(),
        match_oid: local_head.clone(),
        merge_oid: squash_oid.clone(),
    };
    let found = real.verify_squash_merges_with(&candidates, &gh_ok).unwrap();
    assert_eq!(found.get("t_sat"), Some(&local_head));
    assert_eq!(gh_ok.per_head_calls.load(Ordering::SeqCst), 2);

    let gh_rl = SaturatedGh {
        bulk_rows,
        per_head_calls: AtomicUsize::new(0),
        rate_limit: true,
        match_head: "aaf/t_sat".into(),
        match_oid: local_head,
        merge_oid: squash_oid,
    };
    let mut warnings = Vec::new();
    let res = real
        .verify_squash_merges_with_warnings(&candidates, &gh_rl, &mut |w| warnings.push(w))
        .unwrap();
    assert!(res.is_empty());
    assert_eq!(gh_rl.per_head_calls.load(Ordering::SeqCst), 1);
    assert_eq!(warnings.len(), 1);
}

#[test]
fn c1_7_db_compare_and_set_and_complete_review_task_are_atomic() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    db.insert_task(&make_review_task(tmp.path(), "t-cas", Some("fp-1")))
        .unwrap();

    assert!(
        !db.compare_and_set_state("t-cas", TaskState::Ready, TaskState::Running, None, 10)
            .unwrap()
    );
    assert_eq!(db.get_task("t-cas").unwrap().state, TaskState::Review);

    let payload = json!({ "merged": true, "kind": "pr_merge" });
    assert!(complete_review_task(&db, "t-cas", "merged", Some(&payload), 20).unwrap());
    assert_eq!(db.get_task("t-cas").unwrap().state, TaskState::Done);
    assert!(!complete_review_task(&db, "t-cas", "merged again", Some(&payload), 30).unwrap());
}
