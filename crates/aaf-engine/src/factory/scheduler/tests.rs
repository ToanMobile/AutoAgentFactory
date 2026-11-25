use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::config::RepoConfig;
use aaf_domain::gate::{
    GateMode, GateStatus, HeadLockReport, LockStatus, RECEIPT_SCHEMA_VERSION, Receipt,
    StoreComplianceReport,
};
use aaf_domain::ports::{
    AgentOutcome, AgentRequest, AgentRunner, Clock, LimitHit, PortError, PortResult, TaskStore,
};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};

use super::{Deps, Hooks, LoopState, Mode, ProviderKey, RunSummary, Scheduler, run};
use crate::cross_audit::build_checklist;
use crate::db::Db;
use crate::factory::Settings;
use crate::factory::ports::{AccountPool, GateRunner, GitOps, Grant, Integration, Notifier};
use crate::git::{MergeOutcome, PrOutcome, TaskWorktree};
use crate::unblocker::QuotaRoute;
use crate::webhook::WebhookEvent;

struct TestClock(AtomicI64);

impl Clock for TestClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Default)]
struct PoolState {
    leased: HashSet<String>,
    cooldown: HashMap<String, i64>,
}

struct Lease {
    id: String,
    state: Arc<Mutex<PoolState>>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.state.lock().unwrap().leased.remove(&self.id);
    }
}

struct TwoAccountPool {
    accounts: Vec<AccountProfile>,
    clock: Arc<TestClock>,
    state: Arc<Mutex<PoolState>>,
}

impl TwoAccountPool {
    fn new(clock: Arc<TestClock>) -> Self {
        let accounts = ["a", "b"]
            .into_iter()
            .map(|name| AccountProfile {
                id: name.to_string(),
                provider: ProviderKind::ClaudeCode,
                config_dir: format!("/fake/home/.claude-{name}"),
                name: name.to_string(),
            })
            .collect();
        Self {
            accounts,
            clock,
            state: Arc::default(),
        }
    }
}

impl AccountPool for TwoAccountPool {
    fn acquire(&self) -> PortResult<Option<Grant>> {
        let now = self.clock.now();
        let mut state = self.state.lock().unwrap();
        for account in &self.accounts {
            let cooling = state.cooldown.get(&account.id).is_some_and(|u| *u > now);
            if cooling || state.leased.contains(&account.id) {
                continue;
            }
            state.leased.insert(account.id.clone());
            let lease = Lease {
                id: account.id.clone(),
                state: Arc::clone(&self.state),
            };
            return Ok(Some(Grant::new(
                Some(account.clone()),
                Some(Box::new(lease)),
            )));
        }
        Ok(None)
    }

    fn after_limit(&self) -> PortResult<Option<QuotaRoute>> {
        let now = self.clock.now();
        let state = self.state.lock().unwrap();
        let any_free = self
            .accounts
            .iter()
            .any(|a| !state.cooldown.get(&a.id).is_some_and(|u| *u > now));
        Ok(Some(if any_free {
            QuotaRoute::Switch
        } else {
            QuotaRoute::Wait
        }))
    }

    fn report_limit(&self, account_id: &str, hit: &LimitHit) -> PortResult<()> {
        let until = hit.reset_at.unwrap_or(self.clock.now() + 18_000);
        self.state
            .lock()
            .unwrap()
            .cooldown
            .insert(account_id.to_string(), until);
        Ok(())
    }

    fn profiles(&self) -> PortResult<Vec<AccountProfile>> {
        Ok(self.accounts.clone())
    }
}

struct StubNotifier;

impl Notifier for StubNotifier {
    fn notify(&self, _event: &WebhookEvent) -> Result<bool, String> {
        Ok(true)
    }
}

struct StubGit {
    root: PathBuf,
}

impl GitOps for StubGit {
    fn task_worktree(&self, task_id: &str) -> PortResult<TaskWorktree> {
        let path = self.root.join(format!("wt-{task_id}"));
        fs::create_dir_all(&path).map_err(|e| PortError::Io(e.to_string()))?;
        Ok(TaskWorktree {
            path,
            branch: format!("aaf/{task_id}"),
        })
    }

    fn commit_all(&self, _worktree: &TaskWorktree, _message: &str) -> PortResult<()> {
        Ok(())
    }

    fn has_changes(&self, _worktree: &TaskWorktree) -> PortResult<bool> {
        Ok(true)
    }

    fn diff(&self, _worktree: &TaskWorktree) -> PortResult<String> {
        Ok("diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n".to_string())
    }

    fn integrate(&self, task_ids: &[String]) -> PortResult<Integration> {
        let path = self.root.join("clean");
        fs::create_dir_all(&path).map_err(|e| PortError::Io(e.to_string()))?;
        Ok(Integration {
            tree: TaskWorktree {
                path,
                branch: "aaf/batch-0".to_string(),
            },
            merged: task_ids.to_vec(),
            conflicted: Vec::new(),
        })
    }

    fn discard(&self, _integration: &Integration) -> PortResult<()> {
        Ok(())
    }

    fn rebase(&self, _worktree: &TaskWorktree) -> PortResult<Vec<String>> {
        Ok(Vec::new())
    }

    fn base_sha(&self) -> PortResult<String> {
        Ok("base0".to_string())
    }

    fn merge(
        &self,
        worktree: &TaskWorktree,
        regate: &dyn Fn(&Path) -> Result<bool, String>,
    ) -> PortResult<MergeOutcome> {
        if !regate(&worktree.path).map_err(PortError::Io)? {
            return Ok(MergeOutcome::GateFailed);
        }
        Ok(MergeOutcome::Merged {
            new_base_sha: "base1".to_string(),
        })
    }

    fn open_pr(
        &self,
        _worktree: &TaskWorktree,
        _title: &str,
        _receipt: &Receipt,
        _note: Option<&str>,
    ) -> PortResult<PrOutcome> {
        Ok(PrOutcome::Skipped {
            reason: "test".to_string(),
        })
    }

    fn fetch_base(&self) -> PortResult<()> {
        Ok(())
    }

    fn is_merged(&self, _task_id: &str) -> PortResult<bool> {
        Ok(false)
    }

    fn cleanup(&self, _task_id: &str) -> PortResult<()> {
        Ok(())
    }
}

struct StubGate {
    data_dir: PathBuf,
    gate_calls: AtomicU32,
}

impl StubGate {
    fn receipt(tree: &Path, status: GateStatus, reasons: Vec<String>) -> Receipt {
        Receipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            repo_root: tree.to_string_lossy().into_owned(),
            tree_fp: "git-tree:t1".to_string(),
            head_sha: None,
            base_sha: None,
            mode: GateMode::Scoped,
            status,
            reasons,
            config_hash: String::new(),
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
        }
    }
}

impl GateRunner for StubGate {
    fn run(&self, tree: &Path) -> PortResult<Receipt> {
        let call = self.gate_calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            Ok(Self::receipt(
                tree,
                GateStatus::Blocked,
                vec!["command test_scoped failed (exit 1)".to_string()],
            ))
        } else {
            Ok(Self::receipt(tree, GateStatus::Clear, Vec::new()))
        }
    }

    fn head_lock(&self, tree: &Path) -> PortResult<HeadLockReport> {
        let (status, lock_fp) = if self.gate_calls.load(Ordering::SeqCst) == 0 {
            (LockStatus::Clear, String::new())
        } else if aaf_fp::approval_path(&self.data_dir, tree, "fp-1").is_file() {
            (LockStatus::Approved, "fp-1".to_string())
        } else {
            (LockStatus::Blocked, "fp-1".to_string())
        };
        Ok(HeadLockReport {
            status,
            lock_fp,
            locked_changes: Vec::new(),
            approval: None,
        })
    }
}

fn ok_outcome(final_text: &str) -> AgentOutcome {
    AgentOutcome {
        exit_code: Some(0),
        timed_out: false,
        final_text: final_text.to_string(),
        tokens_in: 10,
        tokens_out: 5,
        limit_hit: None,
        infra: None,
        log_tail: String::new(),
    }
}

fn limit_outcome(reset_at: i64) -> AgentOutcome {
    AgentOutcome {
        exit_code: Some(1),
        limit_hit: Some(LimitHit {
            reset_at: Some(reset_at),
            raw: "usage limit reached".to_string(),
        }),
        ..ok_outcome("")
    }
}

fn pass_audit_json(spec: &ImmutableSpec) -> String {
    let answers: Vec<serde_json::Value> = build_checklist(spec, false)
        .iter()
        .map(|item| {
            serde_json::json!({
                "id": item.id,
                "answer": "no",
                "evidence": "",
            })
        })
        .collect();
    serde_json::json!({ "answers": answers }).to_string()
}

struct ScriptedRunner {
    clock: Arc<TestClock>,
    worker_outcomes: Mutex<VecDeque<AgentOutcome>>,
    audit_outcomes: Mutex<VecDeque<AgentOutcome>>,
    worker_calls: AtomicU32,
    audit_calls: AtomicU32,
}

impl AgentRunner for ScriptedRunner {
    fn run(&self, request: &AgentRequest) -> PortResult<AgentOutcome> {
        let is_audit = request
            .prompt
            .contains(crate::cross_audit::prompt::SPEC_BEGIN);
        let outcome = if is_audit {
            self.audit_calls.fetch_add(1, Ordering::SeqCst);
            self.audit_outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| ok_outcome(""))
        } else {
            self.worker_calls.fetch_add(1, Ordering::SeqCst);
            self.worker_outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| ok_outcome("done"))
        };
        if outcome.limit_hit.is_none() {
            self.clock.0.fetch_add(20, Ordering::SeqCst);
        }
        Ok(outcome)
    }
}

fn test_spec() -> ImmutableSpec {
    ImmutableSpec {
        description: "Handle empty input cleanly.".to_string(),
        acceptance: vec!["empty input returns empty output".to_string()],
    }
}

#[test]
fn restarts_reset_after_healthy_turn_in_auto_mode_allows_future_switches() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    let data_dir = root.join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let db = Db::open(&data_dir.join("aaf.db")).unwrap();
    let spec = test_spec();
    let mut task = Task::new_root("t1", "/fake/repo", "task t1", spec.clone(), Priority::P2, 1);
    task.state = TaskState::Ready;
    db.insert_task(&task).unwrap();

    let clock = Arc::new(TestClock(AtomicI64::new(1_000)));
    let pool = Arc::new(TwoAccountPool::new(Arc::clone(&clock)));
    let runner = Arc::new(ScriptedRunner {
        clock: Arc::clone(&clock),
        // Round 1: rate-limit switch (#1) -> healthy turn (resets restarts) -> gate fail -> Ready.
        // Round 2: rate-limit switch (#2) -> healthy turn (resets restarts) -> head-lock audit.
        worker_outcomes: Mutex::new(VecDeque::from([
            limit_outcome(1_010),
            ok_outcome("round 1"),
            limit_outcome(1_030),
            ok_outcome("round 2"),
        ])),
        // Audit hits rate-limit switch (#3, > account count 2): only succeeds because healthy
        // turns reset `state.restarts` so the audit can switch to the free second account.
        audit_outcomes: Mutex::new(VecDeque::from([
            limit_outcome(1_050),
            ok_outcome(&pass_audit_json(&spec)),
        ])),
        worker_calls: AtomicU32::new(0),
        audit_calls: AtomicU32::new(0),
    });
    let deps = Deps {
        store: Arc::new(db.clone()),
        clock: Arc::clone(&clock) as Arc<dyn Clock>,
        runner: Arc::clone(&runner) as Arc<dyn AgentRunner>,
        accounts: pool,
        git: Arc::new(StubGit { root }),
        gate: Arc::new(StubGate {
            data_dir: data_dir.clone(),
            gate_calls: AtomicU32::new(0),
        }),
        notifier: Arc::new(StubNotifier),
        settings: Settings {
            repo_root: "/fake/repo".to_string(),
            repo: RepoConfig::default(),
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
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };

    let summary = run(&deps, Mode::Auto, &hooks).unwrap();

    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    assert_eq!(summary.done, vec!["t1"]);
    assert_eq!(db.get_task("t1").unwrap().state, TaskState::Done);
    assert_eq!(runner.worker_calls.load(Ordering::SeqCst), 4);
    assert_eq!(runner.audit_calls.load(Ordering::SeqCst), 2);
}

#[test]
fn expire_waits_clears_restarts_when_cooldown_elapses() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let db = Db::open(&data_dir.join("aaf.db")).unwrap();
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
        parked: HashMap::from([
            ("t1".to_string(), work.clone()),
            ("t2".to_string(), work.clone()),
        ]),
        gating_in_flight: HashSet::new(),
        state: LoopState::default(),
        summary: RunSummary::default(),
    };
    scheduler
        .state
        .restarts
        .insert(("t1".to_string(), work.clone()), 2);
    scheduler
        .state
        .restarts
        .insert(("t2".to_string(), work.clone()), 2);
    scheduler
        .state
        .task_wait_until
        .insert("t1".to_string(), 1_050);
    scheduler
        .state
        .default_wait_until
        .insert(work.clone(), 1_100);

    clock.0.store(1_049, Ordering::SeqCst);
    scheduler.expire_waits();
    assert_eq!(
        scheduler
            .state
            .restarts
            .get(&("t1".to_string(), work.clone())),
        Some(&2)
    );
    assert_eq!(
        scheduler
            .state
            .restarts
            .get(&("t2".to_string(), work.clone())),
        Some(&2)
    );
    assert!(scheduler.parked.contains_key("t1"));

    clock.0.store(1_050, Ordering::SeqCst);
    scheduler.expire_waits();
    assert_eq!(
        scheduler
            .state
            .restarts
            .get(&("t1".to_string(), work.clone())),
        None
    );
    assert!(!scheduler.parked.contains_key("t1"));
    assert!(!scheduler.state.task_wait_until.contains_key("t1"));
    assert_eq!(
        scheduler
            .state
            .restarts
            .get(&("t2".to_string(), work.clone())),
        Some(&2)
    );

    clock.0.store(1_100, Ordering::SeqCst);
    scheduler.expire_waits();
    assert_eq!(
        scheduler
            .state
            .restarts
            .get(&("t2".to_string(), work.clone())),
        None
    );
    assert!(scheduler.parked.is_empty());
    assert!(!scheduler.state.default_wait_until.contains_key(&work));
}
mod park;
