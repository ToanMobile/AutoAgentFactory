//! The test world: every fake wired around a real `Db` in a temp dir (so
//! state transitions are checked by the real store).

use std::path::PathBuf;
use std::sync::atomic::AtomicI64;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use aaf_domain::config::RepoConfig;
use aaf_domain::ports::TaskStore;
use aaf_domain::task::{Priority, Task, TaskState};
use tempfile::TempDir;

use super::fakes::{
    FakeClock, FakeGate, FakeGit, FakeNotifier, FakePool, FakeRunner, GateState, Latch, REPO,
    account, spec,
};
use crate::db::Db;
use crate::factory::{Deps, Hooks, Mode, RunSummary, Settings, run};

/// Long enough that only a hung loop reaches it.
const HANG: Duration = Duration::from_secs(30);

pub struct World {
    pub tmp: TempDir,
    pub db: Db,
    pub clock: Arc<FakeClock>,
    pub git: Arc<FakeGit>,
    pub gate: Arc<FakeGate>,
    pub runner: Arc<FakeRunner>,
    pub pool: Arc<FakePool>,
    pub notifier: Arc<FakeNotifier>,
    pub auto_merge: bool,
    pub max_parallel: u32,
    pub repo: RepoConfig,
}

impl World {
    pub fn with_accounts(accounts: &[&str]) -> World {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
        let clock = Arc::new(FakeClock(AtomicI64::new(1_000)));
        let git = Arc::new(FakeGit::default());
        let gate = Arc::new(FakeGate {
            git: Arc::clone(&git),
            data_dir: tmp.path().to_path_buf(),
            state: Mutex::new(GateState::default()),
        });
        let pool = Arc::new(FakePool {
            accounts: accounts.iter().map(|name| account(name)).collect(),
            clock: Arc::clone(&clock),
            state: Arc::default(),
        });
        World {
            tmp,
            db,
            clock,
            git,
            gate,
            runner: Arc::default(),
            pool,
            notifier: Arc::default(),
            auto_merge: false,
            max_parallel: 1,
            repo: RepoConfig::default(),
        }
    }

    /// One account, named `a`.
    pub fn new() -> World {
        World::with_accounts(&["a"])
    }

    pub fn deps(&self) -> Deps {
        Deps {
            store: Arc::new(self.db.clone()),
            clock: self.clock.clone(),
            runner: self.runner.clone(),
            accounts: self.pool.clone(),
            git: self.git.clone(),
            gate: self.gate.clone(),
            notifier: self.notifier.clone(),
            settings: Settings {
                repo_root: REPO.to_string(),
                repo: self.repo.clone(),
                max_parallel: self.max_parallel,
                auto_merge: self.auto_merge,
                sandboxed: false,
                // The fakes have no files; `aaf run` tests cover the check.
                hook_required: false,
                agent_path: Default::default(),
                mobile: false,
                data_dir: self.tmp.path().to_path_buf(),
                home: PathBuf::from("/fake/home"),
            },
        }
    }

    /// Adds a Ready root task.
    pub fn add(&self, id: &str) -> Task {
        let mut task = Task::new_root(id, REPO, format!("task {id}"), spec(), Priority::P2, 1);
        task.state = TaskState::Ready;
        self.db.insert_task(&task).unwrap();
        task
    }

    pub fn task(&self, id: &str) -> Task {
        self.db.get_task(id).unwrap()
    }

    pub fn state(&self, id: &str) -> TaskState {
        self.task(id).state
    }

    pub fn run(&self, mode: Mode, idle: &(dyn Fn() -> bool + Sync)) -> RunSummary {
        let hooks = Hooks {
            idle,
            reap: &|_| Ok(()),
        };
        run(&self.deps(), mode, &hooks).unwrap()
    }

    pub fn once(&self) -> RunSummary {
        self.run(Mode::Once, &|| false)
    }

    /// Adds t1..t4 and runs the loop once so that the gate is stuck on t1
    /// while t2 and t3 queue up behind it: the second gate round is exactly
    /// `[t2, t3]`. `stuck` runs while the gate is stuck.
    ///
    /// Ordered by channels only. With one worker slot, t4's agent starting
    /// proves that t3 was handed to the gate queue; t4 is then held until
    /// the `[t2, t3]` round has begun, so it cannot join that batch.
    pub fn once_with_queued_batch(&self, stuck: impl FnOnce()) -> RunSummary {
        for id in ["t1", "t2", "t3", "t4"] {
            self.add(id);
        }
        let (entered_tx, entered) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let (started_tx, started) = mpsc::channel();
        let (ran_tx, ran) = mpsc::channel();
        let (go, go_rx) = mpsc::channel();
        {
            let mut gate = self.gate.state.lock().unwrap();
            gate.latch = Some(Latch {
                entered: entered_tx,
                release: release_rx,
            });
            gate.ran = Some(ran_tx);
            let mut runner = self.runner.0.lock().unwrap();
            runner.started = Some(started_tx);
            runner.hold.insert("t4".to_string(), go_rx);
        }
        std::thread::scope(|scope| {
            let handle = scope.spawn(|| self.once());
            assert_eq!(started.recv_timeout(HANG).unwrap(), "t1");
            entered.recv_timeout(HANG).unwrap();
            for id in ["t2", "t3", "t4"] {
                assert_eq!(started.recv_timeout(HANG).unwrap(), id);
            }
            stuck();
            release.send(()).unwrap();
            while ran.recv_timeout(HANG).unwrap() != ["t2", "t3"] {}
            go.send(()).unwrap();
            handle.join().unwrap()
        })
    }
}
