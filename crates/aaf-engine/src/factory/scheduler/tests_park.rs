//! Parking of tasks that exhausted every account of a provider.

use super::*;

/// A task that ran out of account restarts is parked with a cooldown deadline,
/// so `expire_waits` frees it while other work keeps the loop busy (it used to
/// wait for a fully idle loop and starved under steady load).
#[test]
fn exhausted_switch_parks_with_deadline_and_unparks_while_busy() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let db = Db::open(&data_dir.join("aaf.db")).unwrap();
    let mut task = Task::new_root("t1", "/fake/repo", "task t1", test_spec(), Priority::P2, 1);
    task.state = TaskState::Ready;
    db.insert_task(&task).unwrap();
    let clock = Arc::new(TestClock(AtomicI64::new(1_000)));
    let deps = Deps {
        store: Arc::new(db),
        clock: Arc::clone(&clock) as Arc<dyn Clock>,
        runner: Arc::new(ScriptedRunner {
            clock: Arc::clone(&clock),
            worker_outcomes: Mutex::new(VecDeque::new()),
            audit_outcomes: Mutex::new(VecDeque::new()),
            worker_calls: AtomicU32::new(0),
            audit_calls: AtomicU32::new(0),
        }),
        accounts: Arc::new(TwoAccountPool::new(Arc::clone(&clock))),
        git: Arc::new(StubGit {
            root: tmp.path().to_path_buf(),
        }),
        gate: Arc::new(StubGate {
            data_dir: data_dir.clone(),
            gate_calls: AtomicU32::new(0),
        }),
        notifier: Arc::new(StubNotifier),
        settings: Settings {
            repo_root: "/fake/repo".to_string(),
            repo: RepoConfig::default(),
            max_parallel: 2,
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
    let (msg_tx, msg_rx) = mpsc::channel();
    let (gate_tx, _gate_rx) = mpsc::channel();
    let work = ProviderKey::from_executor("claude");
    let mut scheduler = Scheduler {
        deps: &deps,
        audit: None,
        router: None,
        mode: Mode::Auto,
        should_stop: &|| false,
        stopping: false,
        msg_tx,
        msg_rx,
        gate_tx,
        parked: HashMap::new(),
        gating_in_flight: HashSet::new(),
        state: LoopState::default(),
        summary: RunSummary::default(),
    };
    // Both accounts already tried: the next switch cannot retry.
    scheduler
        .state
        .restarts
        .insert(("t1".to_string(), work.clone()), 2);

    scheduler
        .handle_switch("t1".to_string(), work.clone())
        .unwrap();

    assert_eq!(scheduler.parked.get("t1"), Some(&work));
    let deadline = 1_000 + super::super::DEFAULT_RATE_LIMIT_COOLDOWN_SECS;
    assert_eq!(scheduler.state.task_wait_until.get("t1"), Some(&deadline));
    assert!(
        scheduler.state.default_wait_until.is_empty(),
        "one task's exhaustion must not cool down the whole provider"
    );

    clock.0.store(deadline, Ordering::SeqCst);
    scheduler.expire_waits();
    assert!(!scheduler.parked.contains_key("t1"));
    assert!(
        !scheduler
            .state
            .restarts
            .contains_key(&("t1".to_string(), work))
    );
}
