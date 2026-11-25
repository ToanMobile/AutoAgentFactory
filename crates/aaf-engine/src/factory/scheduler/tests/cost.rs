//! Scheduler tests for the 24h token cap (auto-resuming pause) and 1h spike
//! detector (hard factory freeze).

mod store_wrap;

use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::pipeline::CostGuardConfig;
use aaf_domain::ports::{KvStore, META_COST_PAUSED_UNTIL, META_FACTORY_FROZEN, RunStore};
use aaf_domain::run::{RunRecord, RunStatus};
use serde_json::Value;

use super::*;
use crate::factory::{Store, is_frozen, unfreeze};
use store_wrap::CountingStore;

struct ClearGate;

impl GateRunner for ClearGate {
    fn run(&self, tree: &Path) -> PortResult<Receipt> {
        Ok(StubGate::receipt(tree, GateStatus::Clear, Vec::new()))
    }

    fn head_lock(&self, _tree: &Path) -> PortResult<HeadLockReport> {
        Ok(HeadLockReport {
            status: LockStatus::Clear,
            lock_fp: String::new(),
            locked_changes: Vec::new(),
            approval: None,
        })
    }
}

fn insert_done_history_task(db: &Db, id: &str) {
    let mut task = Task::new_root(id, "/fake/repo", "history", test_spec(), Priority::P2, 1);
    task.state = TaskState::Done;
    db.insert_task(&task).unwrap();
}

fn insert_ready_task(db: &Db, id: &str) {
    let mut task = Task::new_root(id, "/fake/repo", "ready task", test_spec(), Priority::P2, 1);
    task.state = TaskState::Ready;
    db.insert_task(&task).unwrap();
}

fn insert_run_range(
    db: &Db,
    run_id: &str,
    task_id: &str,
    started_at: i64,
    ended_at: Option<i64>,
    tokens: (u64, u64),
) {
    db.insert_run(&RunRecord {
        id: run_id.to_string(),
        task_id: task_id.to_string(),
        account_id: Some("a".to_string()),
        executor: "claude".to_string(),
        worktree_path: String::new(),
        branch: format!("aaf/{task_id}"),
        status: RunStatus::Succeeded,
        exit_code: Some(0),
        tokens_in: tokens.0,
        tokens_out: tokens.1,
        started_at,
        ended_at,
    })
    .unwrap();
}

fn insert_run_tokens(db: &Db, run_id: &str, task_id: &str, started_at: i64, tokens: (u64, u64)) {
    insert_run_range(
        db,
        run_id,
        task_id,
        started_at,
        Some(started_at + 10),
        tokens,
    );
}

fn build_cost_deps(
    root: PathBuf,
    data_dir: PathBuf,
    store: Arc<dyn Store>,
    clock: Arc<TestClock>,
    cost_guard: CostGuardConfig,
) -> (Deps, Arc<ScriptedRunner>) {
    let runner = Arc::new(ScriptedRunner {
        clock: Arc::clone(&clock),
        worker_outcomes: Mutex::new(VecDeque::new()),
        audit_outcomes: Mutex::new(VecDeque::new()),
        worker_calls: AtomicU32::new(0),
        audit_calls: AtomicU32::new(0),
    });
    let mut repo = RepoConfig::default();
    repo.unblocker.cost_guard = cost_guard;
    let deps = Deps {
        store,
        clock: Arc::clone(&clock) as Arc<dyn Clock>,
        runner: Arc::clone(&runner) as Arc<dyn AgentRunner>,
        accounts: Arc::new(TwoAccountPool::new(clock)),
        git: Arc::new(StubGit { root }),
        gate: Arc::new(ClearGate),
        notifier: Arc::new(StubNotifier),
        settings: Settings {
            repo_root: "/fake/repo".to_string(),
            repo,
            max_parallel: 1,
            auto_merge: true,
            sandboxed: false,
            hook_required: false,
            agent_path: Default::default(),
            mobile: false,
            data_dir,
            home: PathBuf::from("/fake/home"),
            owner_github_allowlist: Vec::new(),
        },
    };
    (deps, runner)
}

fn events_of_kind(db: &Db, kind: EventKind) -> Vec<FactoryEvent> {
    db.events_after(None, 0, 500)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == kind)
        .collect()
}

#[test]
fn daily_cap_pauses_new_work_then_resumes_when_window_rolls() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    let data_dir = root.join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let db = Db::open(&data_dir.join("aaf.db")).unwrap();

    let start_now = 200_000_i64;
    let run_started = start_now - 7_200; // 2h ago: inside 24h cap, outside 1h spike window.
    insert_done_history_task(&db, "hist");
    insert_run_tokens(&db, "r-hist", "hist", run_started, (700, 500));
    insert_ready_task(&db, "t1");

    let clock = Arc::new(TestClock(AtomicI64::new(start_now)));
    let (deps, runner) = build_cost_deps(
        root,
        data_dir,
        Arc::new(db.clone()),
        Arc::clone(&clock),
        CostGuardConfig {
            daily_token_cap: 1_000,
            spike_factor: 4,
        },
    );

    let idle_step = AtomicU32::new(0);
    let hooks = Hooks {
        idle: &|| match idle_step.fetch_add(1, Ordering::SeqCst) {
            0 => {
                assert_eq!(runner.worker_calls.load(Ordering::SeqCst), 0);
                assert_eq!(db.get_task("t1").unwrap().state, TaskState::Ready);
                assert_eq!(events_of_kind(&db, EventKind::CostPaused).len(), 1);
                assert!(!is_frozen(&db).unwrap());
                // Advance past 30s cache while still inside 24h window.
                clock.0.fetch_add(60, Ordering::SeqCst);
                true
            }
            1 => {
                assert_eq!(runner.worker_calls.load(Ordering::SeqCst), 0);
                assert_eq!(events_of_kind(&db, EventKind::CostPaused).len(), 1);
                // Advance +24h so the historical run rolls out of the 24h window.
                clock.0.store(start_now + 86_400, Ordering::SeqCst);
                true
            }
            _ => false,
        },
        reap: &|_| Ok(()),
    };

    let summary = run(&deps, Mode::Auto, &hooks).unwrap();
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    assert_eq!(summary.done, vec!["t1"]);
    assert!(!summary.frozen);
    assert!(!summary.cost_paused);
    assert_eq!(runner.worker_calls.load(Ordering::SeqCst), 1);
    assert_eq!(db.get_meta(META_FACTORY_FROZEN).unwrap(), None);
    assert_eq!(
        db.get_meta(META_COST_PAUSED_UNTIL).unwrap().as_deref(),
        Some("0")
    );

    let paused = events_of_kind(&db, EventKind::CostPaused);
    assert_eq!(paused.len(), 1);
    let paused_json: Value = serde_json::from_str(&paused[0].payload_json).unwrap();
    assert_eq!(paused_json["tokens_24h"], 1_200);
    assert_eq!(paused_json["cap"], 1_000);
    assert_eq!(paused_json["resume_at"], (run_started + 10) + 86_400);

    let resumed = events_of_kind(&db, EventKind::CostResumed);
    assert_eq!(resumed.len(), 1);
    let resumed_json: Value = serde_json::from_str(&resumed[0].payload_json).unwrap();
    assert_eq!(resumed_json["tokens_24h"], 0);
    assert_eq!(resumed_json["cap"], 1_000);
}

#[test]
fn spike_freezes_factory_and_requires_unfreeze() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    let data_dir = root.join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let db = Db::open(&data_dir.join("aaf.db")).unwrap();

    let start_now = 200_000_i64;
    insert_done_history_task(&db, "hist");
    // Cap 24_000, factor 4 -> 1h limit = 4_000. 5_000 tokens in last hour triggers spike.
    insert_run_tokens(&db, "r-spike", "hist", start_now - 600, (3_000, 2_000));
    insert_ready_task(&db, "t1");

    let clock = Arc::new(TestClock(AtomicI64::new(start_now)));
    let (deps, runner) = build_cost_deps(
        root,
        data_dir,
        Arc::new(db.clone()),
        Arc::clone(&clock),
        CostGuardConfig {
            daily_token_cap: 24_000,
            spike_factor: 4,
        },
    );
    let hooks = Hooks {
        idle: &|| true,
        reap: &|_| Ok(()),
    };

    let summary = run(&deps, Mode::Auto, &hooks).unwrap();
    assert!(summary.frozen);
    assert!(!summary.cost_paused);
    assert!(is_frozen(&db).unwrap());
    assert_eq!(runner.worker_calls.load(Ordering::SeqCst), 0);
    assert_eq!(db.get_task("t1").unwrap().state, TaskState::Ready);

    let frozen_events = events_of_kind(&db, EventKind::FactoryFrozen);
    assert_eq!(frozen_events.len(), 1);
    let payload: Value = serde_json::from_str(&frozen_events[0].payload_json).unwrap();
    assert_eq!(payload["reason"], "token_spike");
    assert_eq!(payload["tokens_last_hour"], 5_000);
    assert_eq!(payload["limit_last_hour"], 4_000);
    assert_eq!(payload["consecutive_fails"], 0);
    assert_eq!(payload["threshold"], 0);
    assert!(events_of_kind(&db, EventKind::CostPaused).is_empty());

    // Advance clock +25h: factory MUST remain frozen until human unfreezes.
    clock.0.store(start_now + 25 * 3600, Ordering::SeqCst);
    let second = run(&deps, Mode::Auto, &hooks).unwrap();
    assert!(second.frozen);
    assert!(is_frozen(&db).unwrap());
    assert_eq!(runner.worker_calls.load(Ordering::SeqCst), 0);
    assert!(events_of_kind(&db, EventKind::FactoryUnfrozen).is_empty());
}

#[test]
fn cap_resume_never_clears_consecutive_fail_freeze() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    let data_dir = root.join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let db = Db::open(&data_dir.join("aaf.db")).unwrap();

    let start_now = 200_000_i64;
    insert_done_history_task(&db, "hist");
    insert_run_tokens(&db, "r-cap", "hist", start_now - 7_200, (800, 400));
    insert_ready_task(&db, "t1");

    let clock = Arc::new(TestClock(AtomicI64::new(start_now)));
    let (deps, runner) = build_cost_deps(
        root,
        data_dir,
        Arc::new(db.clone()),
        Arc::clone(&clock),
        CostGuardConfig {
            daily_token_cap: 1_000,
            spike_factor: 4,
        },
    );

    let hooks = Hooks {
        idle: &|| {
            assert_eq!(events_of_kind(&db, EventKind::CostPaused).len(), 1);
            assert!(!is_frozen(&db).unwrap());
            db.set_meta(META_FACTORY_FROZEN, "1").unwrap();
            clock.0.store(start_now + 86_400, Ordering::SeqCst);
            true
        },
        reap: &|_| Ok(()),
    };

    let summary = run(&deps, Mode::Auto, &hooks).unwrap();
    assert!(summary.frozen);
    assert!(is_frozen(&db).unwrap());
    assert_eq!(runner.worker_calls.load(Ordering::SeqCst), 0);
    assert_eq!(db.get_task("t1").unwrap().state, TaskState::Ready);
    assert!(events_of_kind(&db, EventKind::FactoryUnfrozen).is_empty());
}

#[test]
fn disabled_cost_guard_never_queries() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    let data_dir = root.join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let db = Db::open(&data_dir.join("aaf.db")).unwrap();
    insert_ready_task(&db, "t1");

    let queries = Arc::new(AtomicU32::new(0));
    let store = Arc::new(CountingStore {
        inner: db.clone(),
        cost_queries: Arc::clone(&queries),
    });
    let clock = Arc::new(TestClock(AtomicI64::new(200_000)));
    let (deps, runner) = build_cost_deps(root, data_dir, store, clock, CostGuardConfig::default());
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };

    let summary = run(&deps, Mode::Once, &hooks).unwrap();
    assert_eq!(summary.done, vec!["t1"]);
    assert!(!summary.cost_paused);
    assert!(!summary.frozen);
    assert_eq!(runner.worker_calls.load(Ordering::SeqCst), 1);
    assert_eq!(queries.load(Ordering::SeqCst), 0);
    assert!(events_of_kind(&db, EventKind::CostPaused).is_empty());
    assert!(events_of_kind(&db, EventKind::CostResumed).is_empty());
    assert!(events_of_kind(&db, EventKind::FactoryFrozen).is_empty());
}

#[test]
fn paused_events_are_emitted_once_per_transition() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    let data_dir = root.join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let db = Db::open(&data_dir.join("aaf.db")).unwrap();

    let start_now = 200_000_i64;
    insert_done_history_task(&db, "hist");
    insert_run_tokens(&db, "r1", "hist", start_now - 7_200, (600, 600));

    let clock = Arc::new(TestClock(AtomicI64::new(start_now)));
    let (deps, _runner) = build_cost_deps(
        root,
        data_dir,
        Arc::new(db.clone()),
        Arc::clone(&clock),
        CostGuardConfig {
            daily_token_cap: 1_000,
            spike_factor: 4,
        },
    );

    let step = AtomicU32::new(0);
    let hooks = Hooks {
        idle: &|| match step.fetch_add(1, Ordering::SeqCst) {
            0 => {
                assert_eq!(events_of_kind(&db, EventKind::CostPaused).len(), 1);
                assert_eq!(events_of_kind(&db, EventKind::CostResumed).len(), 0);
                clock.0.fetch_add(60, Ordering::SeqCst);
                true
            }
            1 => {
                assert_eq!(events_of_kind(&db, EventKind::CostPaused).len(), 1);
                assert_eq!(events_of_kind(&db, EventKind::CostResumed).len(), 0);
                clock.0.fetch_add(86_400, Ordering::SeqCst);
                true
            }
            2 => {
                assert_eq!(events_of_kind(&db, EventKind::CostPaused).len(), 1);
                assert_eq!(events_of_kind(&db, EventKind::CostResumed).len(), 1);
                clock.0.fetch_add(60, Ordering::SeqCst);
                true
            }
            3 => {
                assert_eq!(events_of_kind(&db, EventKind::CostPaused).len(), 1);
                assert_eq!(events_of_kind(&db, EventKind::CostResumed).len(), 1);
                let now = clock.now();
                insert_run_tokens(&db, "r2", "hist", now - 7_200, (700, 500));
                clock.0.fetch_add(60, Ordering::SeqCst);
                true
            }
            _ => false,
        },
        reap: &|_| Ok(()),
    };

    let summary = run(&deps, Mode::Auto, &hooks).unwrap();
    assert!(summary.cost_paused);
    assert!(!summary.frozen);
    assert_eq!(db.get_meta(META_FACTORY_FROZEN).unwrap(), None);
    assert_eq!(events_of_kind(&db, EventKind::CostPaused).len(), 2);
    assert_eq!(events_of_kind(&db, EventKind::CostResumed).len(), 1);
}

#[test]
fn unfreeze_anchors_spike_window_and_re_freezes_on_new_spike() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    let data_dir = root.join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let db = Db::open(&data_dir.join("aaf.db")).unwrap();

    let start_now = 200_000_i64;
    insert_done_history_task(&db, "hist");
    // Cap 24_000, factor 4 -> 1h limit = 4_000. Old spike ends before unfreeze.
    insert_run_tokens(&db, "r-spike-1", "hist", start_now - 600, (3_000, 2_000));
    insert_ready_task(&db, "t1");

    let clock = Arc::new(TestClock(AtomicI64::new(start_now)));
    let (deps, runner) = build_cost_deps(
        root,
        data_dir,
        Arc::new(db.clone()),
        Arc::clone(&clock),
        CostGuardConfig {
            daily_token_cap: 24_000,
            spike_factor: 4,
        },
    );
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };

    let first = run(&deps, Mode::Once, &hooks).unwrap();
    assert!(first.frozen);
    assert!(is_frozen(&db).unwrap());
    assert_eq!(events_of_kind(&db, EventKind::FactoryFrozen).len(), 1);

    // Human unfreezes within the same hour -> next run MUST NOT immediately re-freeze.
    clock.0.store(start_now + 60, Ordering::SeqCst);
    unfreeze(&db, clock.as_ref()).unwrap();
    clock.0.store(start_now + 120, Ordering::SeqCst);
    let after_unfreeze = run(&deps, Mode::Once, &hooks).unwrap();
    assert!(!after_unfreeze.frozen);
    assert!(!is_frozen(&db).unwrap());
    assert_eq!(after_unfreeze.done, vec!["t1"]);
    assert_eq!(runner.worker_calls.load(Ordering::SeqCst), 1);

    // A NEW run burning above limit after unfreeze MUST freeze again.
    let new_end = clock.now() + 10;
    insert_run_range(
        &db,
        "r-spike-2",
        "hist",
        new_end - 5,
        Some(new_end),
        (2_500, 2_000),
    );
    insert_ready_task(&db, "t2");
    clock.0.store(new_end + 60, Ordering::SeqCst);
    let refrozen = run(&deps, Mode::Once, &hooks).unwrap();
    assert!(refrozen.frozen);
    assert!(is_frozen(&db).unwrap());
    assert_eq!(events_of_kind(&db, EventKind::FactoryFrozen).len(), 2);
    assert_eq!(db.get_task("t2").unwrap().state, TaskState::Ready);
}

#[test]
fn pause_persists_in_meta_across_separate_once_runs_without_spam() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    let data_dir = root.join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let db = Db::open(&data_dir.join("aaf.db")).unwrap();

    let start_now = 200_000_i64;
    insert_done_history_task(&db, "hist");
    insert_run_tokens(&db, "r-cap", "hist", start_now - 7_200, (700, 500));
    insert_ready_task(&db, "t1");

    let clock = Arc::new(TestClock(AtomicI64::new(start_now)));
    let (deps, runner) = build_cost_deps(
        root,
        data_dir,
        Arc::new(db.clone()),
        Arc::clone(&clock),
        CostGuardConfig {
            daily_token_cap: 1_000,
            spike_factor: 4,
        },
    );
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };

    // Two separate `factory::run` invocations (fresh Scheduler each time) while over cap.
    let first = run(&deps, Mode::Once, &hooks).unwrap();
    assert!(first.cost_paused);
    assert!(!first.frozen);
    assert_eq!(db.get_meta(META_FACTORY_FROZEN).unwrap(), None);
    assert_eq!(events_of_kind(&db, EventKind::CostPaused).len(), 1);

    clock.0.store(start_now + 60, Ordering::SeqCst);
    let second = run(&deps, Mode::Once, &hooks).unwrap();
    assert!(second.cost_paused);
    assert!(!second.frozen);
    assert_eq!(db.get_meta(META_FACTORY_FROZEN).unwrap(), None);
    assert_eq!(events_of_kind(&db, EventKind::CostPaused).len(), 1);
    assert_eq!(runner.worker_calls.load(Ordering::SeqCst), 0);

    // After 24h window rolls, a new process resumes and emits CostResumed once.
    clock.0.store(start_now + 86_400, Ordering::SeqCst);
    let third = run(&deps, Mode::Once, &hooks).unwrap();
    assert!(!third.cost_paused);
    assert_eq!(third.done, vec!["t1"]);
    assert_eq!(events_of_kind(&db, EventKind::CostResumed).len(), 1);
    assert_eq!(
        db.get_meta(META_COST_PAUSED_UNTIL).unwrap().as_deref(),
        Some("0")
    );
}

#[test]
fn long_running_task_ending_within_hour_triggers_spike_freeze() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    let data_dir = root.join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let db = Db::open(&data_dir.join("aaf.db")).unwrap();

    let start_now = 200_000_i64;
    insert_done_history_task(&db, "hist");
    // Started 2h ago, finished 60s ago with 5_000 tokens (limit = 4_000) -> MUST freeze.
    insert_run_range(
        &db,
        "r-long",
        "hist",
        start_now - 7_200,
        Some(start_now - 60),
        (3_000, 2_000),
    );
    insert_ready_task(&db, "t1");

    let clock = Arc::new(TestClock(AtomicI64::new(start_now)));
    let (deps, runner) = build_cost_deps(
        root,
        data_dir,
        Arc::new(db.clone()),
        clock,
        CostGuardConfig {
            daily_token_cap: 24_000,
            spike_factor: 4,
        },
    );
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };

    let summary = run(&deps, Mode::Once, &hooks).unwrap();
    assert!(summary.frozen);
    assert!(is_frozen(&db).unwrap());
    assert_eq!(runner.worker_calls.load(Ordering::SeqCst), 0);
    let frozen = events_of_kind(&db, EventKind::FactoryFrozen);
    assert_eq!(frozen.len(), 1);
    let payload: Value = serde_json::from_str(&frozen[0].payload_json).unwrap();
    assert_eq!(payload["reason"], "token_spike");
    assert_eq!(payload["tokens_last_hour"], 5_000);
}
