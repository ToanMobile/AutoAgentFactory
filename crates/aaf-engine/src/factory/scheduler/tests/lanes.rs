//! Regression tests for split `Coding` / `Aux` scheduler lanes (I-6) and
//! zero-lease deterministic `Job::Unblock`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::config::{AuditScope, RepoConfig};
use aaf_domain::gate::{GateStatus, HeadLockReport, LockStatus, Receipt};
use aaf_domain::ports::{AgentOutcome, AgentRequest, AgentRunner, LimitHit, PortResult, TaskStore};
use aaf_domain::task::{Priority, Task, TaskState};
use tempfile::TempDir;

use super::{
    Deps, Hooks, Mode, Settings, StubGate, StubGit, StubNotifier, TestClock, TwoAccountPool,
    ok_outcome, pass_audit_json, run, test_spec,
};
use crate::db::Db;
use crate::factory::ports::{AccountPool, GateRunner, Grant, Purpose};
use crate::unblocker::QuotaRoute;

fn make_task(id: &str, repo: &str, state: TaskState, created_at: i64) -> Task {
    let mut t = Task::new_root(
        id,
        repo,
        format!("task {id}"),
        test_spec(),
        Priority::P1,
        created_at,
    );
    t.state = state;
    t
}

fn base_settings(repo_root: String, data_dir: PathBuf, repo: RepoConfig) -> Settings {
    Settings {
        repo_root,
        repo,
        max_parallel: 1,
        auto_merge: false,
        sandboxed: false,
        hook_required: false,
        agent_path: Default::default(),
        mobile: false,
        data_dir,
        home: PathBuf::from("/fake/home"),
        owner_github_allowlist: Vec::new(),
    }
}

struct GateByTask {
    fail_task: Option<String>,
    wait_for_coding: Option<Arc<(Mutex<bool>, Condvar)>>,
}

impl GateRunner for GateByTask {
    fn run(&self, tree: &Path) -> PortResult<Receipt> {
        if let Some(gate_wait) = &self.wait_for_coding {
            let (lock, cv) = &**gate_wait;
            let guard = lock.lock().unwrap();
            let _ = cv
                .wait_timeout_while(guard, Duration::from_secs(5), |started| !*started)
                .unwrap();
        }
        let name = tree.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if self
            .fail_task
            .as_deref()
            .is_some_and(|id| name.ends_with(id))
        {
            Ok(StubGate::receipt(
                tree,
                GateStatus::Blocked,
                vec!["command test_scoped failed (exit 1)".to_string()],
            ))
        } else {
            Ok(StubGate::receipt(tree, GateStatus::Clear, Vec::new()))
        }
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

/// Holds a coding worker on a `Condvar` until the aux audit completes.
/// Proves that `PostAudit` runs concurrently with `max_parallel = 1` coding.
struct BarrierAuditRunner {
    coding_started: Arc<(Mutex<bool>, Condvar)>,
    audit_done: Arc<(Mutex<bool>, Condvar)>,
    audit_ran_while_coding: Arc<AtomicBool>,
    coding_in_flight: Arc<AtomicBool>,
}

impl AgentRunner for BarrierAuditRunner {
    fn run(&self, request: &AgentRequest) -> PortResult<AgentOutcome> {
        let is_audit = request
            .prompt
            .contains(crate::cross_audit::prompt::SPEC_BEGIN)
            || request.sandbox.read_only;
        if is_audit {
            if self.coding_in_flight.load(Ordering::SeqCst) {
                self.audit_ran_while_coding.store(true, Ordering::SeqCst);
            }
            let (lock, cv) = &*self.audit_done;
            *lock.lock().unwrap() = true;
            cv.notify_all();
            return Ok(ok_outcome(&pass_audit_json(&test_spec())));
        }
        self.coding_in_flight.store(true, Ordering::SeqCst);
        {
            let (lock, cv) = &*self.coding_started;
            *lock.lock().unwrap() = true;
            cv.notify_all();
        }
        let (lock, cv) = &*self.audit_done;
        let guard = lock.lock().unwrap();
        let _ = cv
            .wait_timeout_while(guard, Duration::from_secs(5), |done| !*done)
            .unwrap();
        self.coding_in_flight.store(false, Ordering::SeqCst);
        Ok(ok_outcome("coded"))
    }
}

#[test]
fn aux_audit_runs_while_coding_worker_holds_max_parallel_1() {
    let tmp = TempDir::new().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let repo = root.join("repo");
    let data = root.join("data");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    let db = Db::open(&data.join("aaf.db")).unwrap();
    let repo_str = repo.to_string_lossy().into_owned();

    // t1 is already Gating (needs gate + PostAudit); t2 is Ready (takes coding slot 1/1).
    db.insert_task(&make_task("t1", &repo_str, TaskState::Gating, 1))
        .unwrap();
    db.insert_task(&make_task("t2", &repo_str, TaskState::Ready, 2))
        .unwrap();

    let clock = Arc::new(TestClock(AtomicI64::new(1_000)));
    let pool = Arc::new(TwoAccountPool::new(Arc::clone(&clock)));
    let coding_started = Arc::new((Mutex::new(false), Condvar::new()));
    let audit_done = Arc::new((Mutex::new(false), Condvar::new()));
    let audit_ran_while_coding = Arc::new(AtomicBool::new(false));
    let coding_in_flight = Arc::new(AtomicBool::new(false));
    let runner = Arc::new(BarrierAuditRunner {
        coding_started: Arc::clone(&coding_started),
        audit_done: Arc::clone(&audit_done),
        audit_ran_while_coding: Arc::clone(&audit_ran_while_coding),
        coding_in_flight: Arc::clone(&coding_in_flight),
    });

    let mut repo_cfg = RepoConfig::default();
    repo_cfg.run.max_parallel = 1;
    repo_cfg.cross_audit.audit_scope = AuditScope::Always;
    repo_cfg.cross_audit.score_gate = false;
    let deps = Deps {
        store: Arc::new(db),
        git: Arc::new(StubGit { root: repo }),
        gate: Arc::new(GateByTask {
            fail_task: None,
            wait_for_coding: Some(coding_started),
        }),
        runner,
        accounts: pool,
        notifier: Arc::new(StubNotifier),
        clock,
        settings: base_settings(repo_str, data, repo_cfg),
    };
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };
    let summary = run(&deps, Mode::Once, &hooks).unwrap();
    assert!(
        *audit_done.0.lock().unwrap(),
        "t1 post-audit must complete without deadlocking behind t2 coding turn"
    );
    assert!(
        audit_ran_while_coding.load(Ordering::SeqCst),
        "t1 aux audit must overlap with t2 coding when max_parallel = 1"
    );
    assert!(summary.review.contains(&"t1".to_string()));
}

/// Single-account pool where only 1 lease exists at a time.
struct SingleAccountPool {
    leased: Arc<AtomicBool>,
}

struct SingleLease(Arc<AtomicBool>);
impl Drop for SingleLease {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl AccountPool for SingleAccountPool {
    fn acquire(&self) -> PortResult<Option<Grant>> {
        if self
            .leased
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            Ok(Some(Grant::new(
                Some(AccountProfile {
                    id: "only".to_string(),
                    provider: ProviderKind::ClaudeCode,
                    config_dir: "/fake/.claude-only".to_string(),
                    name: "only".to_string(),
                }),
                Some(Box::new(SingleLease(Arc::clone(&self.leased)))),
            )))
        } else {
            Ok(None)
        }
    }
    fn after_limit(&self) -> PortResult<Option<QuotaRoute>> {
        Ok(Some(QuotaRoute::Wait))
    }
    fn report_limit(&self, _account_id: &str, _hit: &LimitHit) -> PortResult<()> {
        Ok(())
    }
    fn profiles(&self) -> PortResult<Vec<AccountProfile>> {
        Ok(vec![AccountProfile {
            id: "only".to_string(),
            provider: ProviderKind::ClaudeCode,
            config_dir: "/fake/.claude-only".to_string(),
            name: "only".to_string(),
        }])
    }
}

struct WaitForUnblockRunner {
    db: Db,
    t1_requeued_during_coding: Arc<AtomicBool>,
    calls: AtomicUsize,
}

impl AgentRunner for WaitForUnblockRunner {
    fn run(&self, _request: &AgentRequest) -> PortResult<AgentOutcome> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            for _ in 0..100 {
                if self.db.get_task("t1").unwrap().state == TaskState::Ready {
                    self.t1_requeued_during_coding.store(true, Ordering::SeqCst);
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        Ok(ok_outcome("done"))
    }
}

#[test]
fn deterministic_unblock_runs_without_account_lease_while_single_account_busy() {
    let tmp = TempDir::new().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let repo = root.join("repo");
    let data = root.join("data");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    let db = Db::open(&data.join("aaf.db")).unwrap();
    let repo_str = repo.to_string_lossy().into_owned();

    db.insert_task(&make_task("t1", &repo_str, TaskState::Gating, 1))
        .unwrap();
    db.insert_task(&make_task("t2", &repo_str, TaskState::Ready, 2))
        .unwrap();

    let clock = Arc::new(TestClock(AtomicI64::new(1_000)));
    let pool = Arc::new(SingleAccountPool {
        leased: Arc::new(AtomicBool::new(false)),
    });
    let t1_requeued = Arc::new(AtomicBool::new(false));
    let runner = Arc::new(WaitForUnblockRunner {
        db: db.clone(),
        t1_requeued_during_coding: Arc::clone(&t1_requeued),
        calls: AtomicUsize::new(0),
    });

    let mut repo_cfg = RepoConfig::default();
    repo_cfg.run.max_parallel = 1;
    let deps = Deps {
        store: Arc::new(db),
        git: Arc::new(StubGit { root: repo }),
        gate: Arc::new(GateByTask {
            fail_task: Some("t1".to_string()),
            wait_for_coding: None,
        }),
        runner,
        accounts: pool,
        notifier: Arc::new(StubNotifier),
        clock,
        settings: base_settings(repo_str, data, repo_cfg),
    };
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };
    let _ = run(&deps, Mode::Once, &hooks).unwrap();
    assert!(
        t1_requeued.load(Ordering::SeqCst),
        "deterministic T3 unblock must move t1 to Ready while t2 holds the sole account lease"
    );
}

/// Pool that refuses `Purpose::Audit` for `t1` (simulating author-exclusion)
/// while still granting `Purpose::Work` for `t2`.
struct AuditRefuseForT1Pool {
    inner: TwoAccountPool,
}

impl AccountPool for AuditRefuseForT1Pool {
    fn acquire(&self) -> PortResult<Option<Grant>> {
        self.inner.acquire()
    }
    fn acquire_for(&self, task_id: &str, purpose: Purpose) -> PortResult<Option<Grant>> {
        if task_id == "t1" && purpose == Purpose::Audit {
            return Ok(None);
        }
        self.inner.acquire()
    }
    fn after_limit(&self) -> PortResult<Option<QuotaRoute>> {
        self.inner.after_limit()
    }
    fn report_limit(&self, account_id: &str, hit: &LimitHit) -> PortResult<()> {
        self.inner.report_limit(account_id, hit)
    }
    fn profiles(&self) -> PortResult<Vec<AccountProfile>> {
        self.inner.profiles()
    }
}

#[test]
fn audit_non_author_wait_does_not_poison_exhausted_for_work() {
    let tmp = TempDir::new().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let repo = root.join("repo");
    let data = root.join("data");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    let db = Db::open(&data.join("aaf.db")).unwrap();
    let repo_str = repo.to_string_lossy().into_owned();

    db.insert_task(&make_task("t1", &repo_str, TaskState::Gating, 1))
        .unwrap();
    db.insert_task(&make_task("t2", &repo_str, TaskState::Ready, 2))
        .unwrap();

    let clock = Arc::new(TestClock(AtomicI64::new(1_000)));
    let pool = Arc::new(AuditRefuseForT1Pool {
        inner: TwoAccountPool::new(Arc::clone(&clock)),
    });
    let t2_ran = Arc::new(AtomicBool::new(false));
    struct SimpleRunner(Arc<AtomicBool>);
    impl AgentRunner for SimpleRunner {
        fn run(&self, _r: &AgentRequest) -> PortResult<AgentOutcome> {
            self.0.store(true, Ordering::SeqCst);
            Ok(ok_outcome("ok"))
        }
    }

    let mut repo_cfg = RepoConfig::default();
    repo_cfg.run.max_parallel = 1;
    repo_cfg.cross_audit.audit_scope = AuditScope::Always;
    let deps = Deps {
        store: Arc::new(db),
        git: Arc::new(StubGit { root: repo }),
        gate: Arc::new(GateByTask {
            fail_task: None,
            wait_for_coding: None,
        }),
        runner: Arc::new(SimpleRunner(Arc::clone(&t2_ran))),
        accounts: pool,
        notifier: Arc::new(StubNotifier),
        clock,
        settings: base_settings(repo_str, data, repo_cfg),
    };
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };
    let _ = run(&deps, Mode::Once, &hooks).unwrap();
    assert!(
        t2_ran.load(Ordering::SeqCst),
        "t1 audit non-author deferral must not block t2 coding work"
    );
}
