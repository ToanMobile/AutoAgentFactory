#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for Round 6A fixes: `head_lock`, `scheduler`/`sweep`, and `linux_sandbox`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use aaf_domain::account::AccountProfile;
use aaf_domain::blocked::BlockedReason;
use aaf_domain::budget::RootBudget;
use aaf_domain::config::RepoConfig;
use aaf_domain::event::FactoryEvent;
use aaf_domain::gate::{
    GateMode, GateStatus, HeadLockReport, LockStatus, RECEIPT_SCHEMA_VERSION, Receipt,
    StoreComplianceReport,
};
use aaf_domain::ports::{
    AgentOutcome, AgentRequest, AgentRunner, BudgetStore, Clock, ClosedFingerprint, EventSink,
    FingerprintStore, KvStore, LimitHit, PortError, PortResult, RunStore, TaskStore,
};
use aaf_domain::run::{RunFinish, RunRecord};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::executors::linux_sandbox::build_bwrap_args;
use aaf_engine::executors::sandbox::SandboxPaths;
use aaf_engine::factory::{
    AccountPool, Deps, GateRunner, GitOps, Grant, Hooks, Integration, Mode, Notifier, Settings,
    Store, run,
};
use aaf_engine::gate::{self, GateError, head_lock};
use aaf_engine::git::{MergeOutcome, PrOutcome, TaskWorktree};
use aaf_engine::webhook::WebhookEvent;
use tempfile::TempDir;

struct TestClock(AtomicI64);
impl TestClock {
    fn new(now: i64) -> Arc<Self> {
        Arc::new(Self(AtomicI64::new(now)))
    }
}
impl Clock for TestClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}: {:?}", out.stderr);
}

// ---- 1. Lane 2: head_lock::approve_matching subset guard ------------------

#[test]
fn head_lock_approve_matching_refuses_to_sign_unpreviewed_base_lock_fp() {
    let (dir, data) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    fs::create_dir_all(root.join(".autofactory")).unwrap();
    fs::create_dir_all(root.join("tests")).unwrap();
    let base_toml = "[gate]\nbase_ref = \"main\"\n";
    fs::write(root.join(".autofactory/config.toml"), base_toml).unwrap();
    fs::write(root.join("tests/basic_test.rs"), "#[test] fn t() {}\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "init"]);

    // Adversarial PR weakens head_lock_globs to hide an edit to `tests/basic_test.rs`.
    git(root, &["checkout", "-q", "-b", "feat/weaken"]);
    let weakened_toml =
        "[gate]\nbase_ref = \"main\"\nhead_lock_globs = [\".autofactory/config.toml\"]\n";
    fs::write(root.join(".autofactory/config.toml"), weakened_toml).unwrap();
    fs::write(root.join("tests/basic_test.rs"), "// weakened\n").unwrap();
    fs::create_dir_all(root.join(".claude/hooks")).unwrap();
    fs::write(root.join(".claude/hooks/local.sh"), "#!/bin/sh\n").unwrap();

    let base_cfg: RepoConfig = toml::from_str(base_toml).unwrap();
    let pr_cfg: RepoConfig = toml::from_str(weakened_toml).unwrap();
    let clock = TestClock::new(100);
    let eval = |cfg: &RepoConfig| head_lock::evaluate(data.path(), root, cfg, "main");
    let approve = |cfg: &RepoConfig, fp: &str| {
        head_lock::approve_matching(data.path(), root, cfg, "rev", false, fp, clock.as_ref())
    };

    let preview = eval(&pr_cfg);
    assert_eq!(preview.status, LockStatus::Blocked);
    assert_eq!(preview.locked_changes.len(), 1);
    assert_eq!(preview.locked_changes[0].path, ".autofactory/config.toml");

    let base_before = eval(&base_cfg);
    assert_eq!(base_before.status, LockStatus::Blocked);
    assert_ne!(base_before.lock_fp, preview.lock_fp);
    assert!(
        base_before
            .locked_changes
            .iter()
            .any(|c| c.path == "tests/basic_test.rs")
    );

    let approval = approve(&pr_cfg, &preview.lock_fp).unwrap();
    assert_eq!(approval.lock_fp, preview.lock_fp);

    // Only `primary_fp` was signed; `base_lock_fp` covering the hidden test edit must NOT be.
    assert_eq!(eval(&pr_cfg).status, LockStatus::Approved);
    assert_eq!(eval(&base_cfg).status, LockStatus::Blocked);
    let key = aaf_fp::load_key(data.path()).unwrap();
    assert!(
        aaf_fp::read_approval(data.path(), &key, root, &base_before.lock_fp)
            .unwrap()
            .is_none()
    );
    fs::remove_dir_all(root.join(".claude")).unwrap();
    assert_eq!(eval(&base_cfg).status, LockStatus::Blocked);

    // Clearing head_lock_globs completely must return NothingLocked under approve_matching.
    let empty_toml = "[gate]\nbase_ref = \"main\"\nhead_lock_globs = []\n";
    fs::write(root.join(".autofactory/config.toml"), empty_toml).unwrap();
    let empty_cfg: RepoConfig = toml::from_str(empty_toml).unwrap();
    let err = approve(&empty_cfg, &eval(&base_cfg).lock_fp).unwrap_err();
    assert!(matches!(err, GateError::NothingLocked), "{err:?}");
    assert_eq!(eval(&base_cfg).status, LockStatus::Blocked);
}

// ---- 2. Lane 5: Scheduler review-dep sweep + empty-review throttling ------

struct CountingStore {
    inner: Db,
    review_queries: AtomicUsize,
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
impl TaskStore for CountingStore {
    fn list_tasks(&self, repo: Option<&str>, state: Option<TaskState>) -> PortResult<Vec<Task>> {
        if state == Some(TaskState::Review) {
            self.review_queries.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.list_tasks(repo, state)
    }
    fwd! {
        fn insert_task(&self, task: &Task) -> PortResult<()>;
        fn get_task(&self, id: &str) -> PortResult<Task>;
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
impl BudgetStore for CountingStore {
    fwd! {
        fn put_budget(&self, b: &RootBudget) -> PortResult<()>;
        fn get_budget(&self, id: &str) -> PortResult<RootBudget>;
        fn add_usage(&self, id: &str, tok: u64, secs: u64, cyc: u32) -> PortResult<RootBudget>;
        fn set_frozen(&self, id: &str, frozen: bool) -> PortResult<()>;
    }
}
impl RunStore for CountingStore {
    fwd! {
        fn insert_run(&self, run: &RunRecord) -> PortResult<()>;
        fn finish_run(&self, id: &str, finish: &RunFinish) -> PortResult<()>;
        fn runs_for_task(&self, task_id: &str) -> PortResult<Vec<RunRecord>>;
        fn tokens_since(&self, account_id: &str, since: i64) -> PortResult<u64>;
    }
}
impl EventSink for CountingStore {
    fwd! {
        fn emit(&self, ev: &FactoryEvent) -> PortResult<i64>;
        fn events_for_task(&self, id: &str, limit: u32) -> PortResult<Vec<FactoryEvent>>;
    }
}
impl KvStore for CountingStore {
    fwd! {
        fn get_meta(&self, key: &str) -> PortResult<Option<String>>;
        fn set_meta(&self, key: &str, value: &str) -> PortResult<()>;
    }
}
impl FingerprintStore for CountingStore {
    fwd! {
        fn record_closed(&self, closed: &ClosedFingerprint) -> PortResult<()>;
        fn find_closed_since(&self, fp: &str, repo: &str, since: i64) -> PortResult<Option<ClosedFingerprint>>;
        fn find_open_task(&self, fp: &str, repo: &str) -> PortResult<Option<String>>;
    }
}
impl Store for CountingStore {}

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

struct FakeGit {
    base: PathBuf,
    merged_review: AtomicBool,
    fail_commit: bool,
}
impl GitOps for FakeGit {
    fn task_worktree(&self, id: &str) -> PortResult<TaskWorktree> {
        let path = self.base.join(format!("wt-{id}"));
        fs::create_dir_all(&path).unwrap();
        Ok(TaskWorktree {
            path,
            branch: format!("aaf/{id}"),
        })
    }
    fn commit_all(&self, _: &TaskWorktree, _: &str) -> PortResult<()> {
        if self.fail_commit {
            return Err(PortError::Io("skip".into()));
        }
        Ok(())
    }
    fn integrate(&self, ids: &[String]) -> PortResult<Integration> {
        let path = self.base.join("clean");
        fs::create_dir_all(&path).unwrap();
        let tree = TaskWorktree {
            path,
            branch: "aaf/clean".into(),
        };
        Ok(Integration {
            tree,
            merged: ids.to_vec(),
            conflicted: Vec::new(),
        })
    }
    fn is_merged(&self, id: &str) -> PortResult<bool> {
        Ok(id == "t_rev" && self.merged_review.load(Ordering::SeqCst))
    }
    git_const! {
        fn has_changes(&self, _: &TaskWorktree) -> PortResult<bool> => Ok(true);
        fn diff(&self, _: &TaskWorktree) -> PortResult<String> => Ok("+ok\n".into());
        fn discard(&self, _: &Integration) -> PortResult<()> => Ok(());
        fn rebase(&self, _: &TaskWorktree) -> PortResult<Vec<String>> => Ok(Vec::new());
        fn base_sha(&self) -> PortResult<String> => Ok("base0".into());
        fn fetch_base(&self) -> PortResult<()> => Ok(());
        fn cleanup(&self, _: &str) -> PortResult<()> => Ok(());
        fn merge(&self, _: &TaskWorktree, _: &dyn Fn(&Path) -> Result<bool, String>) -> PortResult<MergeOutcome> => Ok(MergeOutcome::Merged { new_base_sha: "base1".into() });
        fn open_pr(&self, _: &TaskWorktree, _: &str, _: &Receipt, _: Option<&str>) -> PortResult<PrOutcome> => Ok(PrOutcome::Opened { url: "https://example.test/pr/1".into() });
    }
}

struct ClearGate(String);
impl GateRunner for ClearGate {
    fn run(&self, tree: &Path) -> PortResult<Receipt> {
        Ok(Receipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            repo_root: tree.to_string_lossy().into_owned(),
            tree_fp: "fp-clear".into(),
            head_sha: Some("head1".into()),
            base_sha: Some("base0".into()),
            mode: GateMode::Scoped,
            status: GateStatus::Clear,
            reasons: Vec::new(),
            config_hash: self.0.clone(),
            commands: Vec::new(),
            static_findings: Vec::new(),
            suppressions: 0,
            head_lock: self.head_lock(tree)?,
            store_compliance: StoreComplianceReport::not_applicable(),
            created_at: 2000,
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

fn ok_outcome(text: &str) -> AgentOutcome {
    AgentOutcome {
        exit_code: Some(0),
        timed_out: false,
        final_text: text.into(),
        tokens_in: 10,
        tokens_out: 10,
        limit_hit: None,
        infra: None,
        log_tail: String::new(),
    }
}

fn task_with(id: &str, state: TaskState, priority: Priority, scopes: &[&str]) -> Task {
    let spec = ImmutableSpec {
        description: "Round 6 test".into(),
        acceptance: vec!["ok".into()],
    };
    let mut t = Task::new_root(id, "/fake/repo", id, spec, priority, 1);
    t.state = state;
    t.scope_paths = scopes.iter().map(|s| (*s).to_string()).collect();
    t
}

fn stop_hooks() -> Hooks<'static> {
    Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    }
}

fn make_deps(
    tmp: &TempDir,
    store: Arc<dyn Store>,
    clock: Arc<TestClock>,
    runner: Arc<dyn AgentRunner>,
    git: Arc<FakeGit>,
    repo: RepoConfig,
) -> Deps {
    let cfg_hash = gate::config_hash(&repo);
    Deps {
        store,
        clock,
        runner,
        accounts: Arc::new(NullDeps),
        git,
        gate: Arc::new(ClearGate(cfg_hash)),
        notifier: Arc::new(NullDeps),
        settings: Settings {
            repo_root: "/fake/repo".into(),
            repo,
            max_parallel: 2,
            auto_merge: false,
            sandboxed: false,
            hook_required: false,
            agent_path: Default::default(),
            mobile: false,
            data_dir: tmp.path().to_path_buf(),
            home: tmp.path().to_path_buf(),
        },
    }
}

struct OverlapWaitRunner {
    clock: Arc<TestClock>,
    git: Arc<FakeGit>,
    sib_active: AtomicBool,
    down_started: AtomicBool,
}
impl AgentRunner for OverlapWaitRunner {
    fn run(&self, req: &AgentRequest) -> PortResult<AgentOutcome> {
        if req.cwd.ends_with("wt-t_sib") {
            self.sib_active.store(true, Ordering::SeqCst);
            self.git.merged_review.store(true, Ordering::SeqCst);
            self.clock.0.store(1035, Ordering::SeqCst);
            let t0 = Instant::now();
            while !self.down_started.load(Ordering::SeqCst) && t0.elapsed() < Duration::from_secs(8)
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            self.sib_active.store(false, Ordering::SeqCst);
        } else if req.cwd.ends_with("wt-t_down") && self.sib_active.load(Ordering::SeqCst) {
            self.down_started.store(true, Ordering::SeqCst);
        }
        Ok(ok_outcome("done"))
    }
}

#[test]
fn auto_mode_sweeps_review_dep_waiter_even_when_overlap_candidate_exists() {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    for t in [
        task_with("t_rev", TaskState::Review, Priority::P1, &[]),
        task_with("t_sib", TaskState::Ready, Priority::P0, &["src/shared.rs"]),
        task_with(
            "t_overlap",
            TaskState::Ready,
            Priority::P1,
            &["src/shared.rs"],
        ),
        task_with("t_down", TaskState::Ready, Priority::P1, &["src/other.rs"]),
    ] {
        db.insert_task(&t).unwrap();
    }
    db.add_dep("t_down", "t_rev").unwrap();

    let clock = TestClock::new(1000);
    let git = Arc::new(FakeGit {
        base: tmp.path().to_path_buf(),
        merged_review: AtomicBool::new(false),
        fail_commit: false,
    });
    let runner = Arc::new(OverlapWaitRunner {
        clock: clock.clone(),
        git: git.clone(),
        sib_active: AtomicBool::new(false),
        down_started: AtomicBool::new(false),
    });
    let deps = make_deps(
        &tmp,
        Arc::new(db),
        clock,
        runner.clone(),
        git,
        RepoConfig::default(),
    );

    let summary = run(&deps, Mode::Auto, &stop_hooks()).unwrap();
    assert!(
        runner.down_started.load(Ordering::SeqCst),
        "t_down should start while t_sib is active despite t_overlap: {summary:?}"
    );
    assert_eq!(summary.done, vec!["t_rev"]);
    let mut rev = summary.review;
    rev.sort();
    assert_eq!(rev, vec!["t_down", "t_overlap", "t_sib"]);
}

struct AdvancingRunner(Arc<TestClock>);
impl AgentRunner for AdvancingRunner {
    fn run(&self, req: &AgentRequest) -> PortResult<AgentOutcome> {
        for (suffix, ts) in [("wt-t1", 1035), ("wt-t2", 1040), ("wt-t3", 1045)] {
            if req.cwd.ends_with(suffix) {
                self.0.0.store(ts, Ordering::SeqCst);
            }
        }
        Ok(ok_outcome("step"))
    }
}

#[test]
fn maybe_sweep_review_throttles_when_review_list_is_empty() {
    let tmp = TempDir::new().unwrap();
    let store = Arc::new(CountingStore {
        inner: Db::open(&tmp.path().join("aaf.db")).unwrap(),
        review_queries: AtomicUsize::new(0),
    });
    for (id, prio) in [
        ("t1", Priority::P0),
        ("t2", Priority::P1),
        ("t3", Priority::P2),
        ("t4", Priority::P3),
    ] {
        store
            .insert_task(&task_with(id, TaskState::Ready, prio, &[]))
            .unwrap();
    }
    let clock = TestClock::new(1000);
    let git = Arc::new(FakeGit {
        base: tmp.path().to_path_buf(),
        merged_review: AtomicBool::new(false),
        fail_commit: true,
    });
    let mut repo = RepoConfig::default();
    repo.run.max_open_review = 1;
    let deps = make_deps(
        &tmp,
        store.clone(),
        clock.clone(),
        Arc::new(AdvancingRunner(clock)),
        git,
        repo,
    );

    let summary = run(&deps, Mode::Auto, &stop_hooks()).unwrap();
    assert_eq!(summary.blocked.len(), 4, "{summary:?}");
    assert!(summary.waiting_for_review_cap >= 3, "{summary:?}");
    assert_eq!(store.review_queries.load(Ordering::SeqCst), 2);
}

// ---- 3. Lane 7: linux_sandbox leaf & nested symlink canonical masking -----

#[cfg(unix)]
#[test]
fn bwrap_masks_and_binds_canonical_targets_for_leaf_and_nested_symlinks() {
    use std::os::unix::fs::symlink;

    let dir = TempDir::new().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let (links, real, outside) = (root.join("links"), root.join("real"), root.join("outside"));
    let (wt_real, wt_task) = (real.join("wt_store"), real.join("wt_store/repo/t1"));
    let data_real = real.join("aaf_data");
    let (throttle, guest) = (data_real.join("throttle"), data_real.join("throttle/guest"));
    let file_real = real.join("secret.key");
    let (out_file, out_dir) = (outside.join("escaped.key"), outside.join("escaped_dir"));

    for d in [&links, &wt_task, &guest, &out_dir] {
        fs::create_dir_all(d).unwrap();
    }
    fs::write(data_real.join("receipt.key"), "hmac").unwrap();
    fs::write(&file_real, "secret").unwrap();
    fs::write(&out_file, "escaped").unwrap();

    let (wt_link, data_link, file_link) = (
        links.join("worktrees"),
        links.join("aaf_data"),
        links.join("secret.key"),
    );
    let (nest_file, nest_dir) = (data_real.join("nested_file"), data_real.join("nested_dir"));
    for (src, dst) in [
        (&wt_real, &wt_link),
        (&data_real, &data_link),
        (&file_real, &file_link),
        (&out_file, &nest_file),
        (&out_dir, &nest_dir),
    ] {
        symlink(src, dst).unwrap();
    }

    let s = |p: &Path| p.to_string_lossy().into_owned();
    let paths = SandboxPaths {
        writable: vec![s(&root)],
        worktree: Some(s(&wt_task)),
        worktrees_root: Some(s(&wt_link)),
        read_denied: vec![s(&data_link), s(&file_link), s(&nest_file), s(&nest_dir)],
        ..SandboxPaths::default()
    };
    let argv = build_bwrap_args(&paths, "/bin/true", &[]).unwrap();
    let pos2 = |a: &str, b: &str| argv.windows(2).position(|w| w[0] == a && w[1] == b);
    let pos3 = |a: &str, b: &str, c: &str| {
        argv.windows(3)
            .position(|w| w[0] == a && w[1] == b && w[2] == c)
    };

    let real_pin = pos3("--bind", &s(&real), &s(&real)).unwrap();
    let wt_ro = pos3("--ro-bind", &s(&wt_real), &s(&wt_real)).unwrap();
    let wt_rw = pos3("--bind", &s(&wt_task), &s(&wt_task)).unwrap();
    assert!(real_pin < wt_ro && wt_ro < wt_rw, "{argv:?}");

    let data_tmpfs = pos2("--tmpfs", &s(&data_real)).unwrap();
    let throttle_ro = pos3("--ro-bind", &s(&throttle), &s(&throttle)).unwrap();
    let guest_rw = pos3("--bind", &s(&guest), &s(&guest)).unwrap();
    assert!(
        data_tmpfs < throttle_ro && throttle_ro < guest_rw,
        "{argv:?}"
    );
    assert!(pos3("--ro-bind", "/dev/null", &s(&file_real)).is_some());

    let out_pin = pos3("--bind", &s(&outside), &s(&outside)).unwrap();
    assert!(out_pin < pos3("--ro-bind", "/dev/null", &s(&out_file)).unwrap());
    assert!(out_pin < pos2("--tmpfs", &s(&out_dir)).unwrap());

    for sym in [
        &wt_link,
        &data_link,
        &data_link.join("throttle"),
        &file_link,
        &nest_file,
        &nest_dir,
    ] {
        assert!(
            !argv.contains(&s(sym)),
            "leaf symlink leaked: {sym:?}\n{argv:?}"
        );
    }
}
