#![allow(clippy::unwrap_used, clippy::expect_used)]
//! V3.2 Phase 1 self-healing, structured 5xx/overloaded detection, transient
//! backoff, and `max_open_review` queue-brake integration tests.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::config::RepoConfig;
use aaf_domain::gate::{
    GateMode, GateStatus, HeadLockReport, LockStatus, RECEIPT_SCHEMA_VERSION, Receipt,
    StoreComplianceReport,
};
use aaf_domain::ports::{
    AgentOutcome, AgentRequest, AgentRunner, Clock, InfraFailure, InfraKind, LimitHit, PortResult,
    TaskStore,
};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::executors::claude::parse_stream;
use aaf_engine::factory::{
    AccountPool, Deps, GateRunner, GitOps, Grant, Hooks, Integration, MAX_TRANSIENT_BACKOFF_SECS,
    MAX_TRANSIENT_RETRIES, Mode, Notifier, Settings, run, transient_backoff_secs,
};
use aaf_engine::git::{MergeOutcome, PrOutcome, TaskWorktree};
use aaf_engine::webhook::WebhookEvent;
use serde_json::json;
use tempfile::TempDir;

const REPO: &str = "/fake/v32-repo";

// ---- structured stream-json parsing tests ---------------------------------

#[test]
fn claude_structured_5xx_and_overloaded_map_to_network() {
    let cases = [
        json!({"type": "result", "is_error": true, "result": "Internal error", "api_error_status": 500})
            .to_string(),
        json!({"type": "result", "is_error": true, "result": "Unavailable", "status_code": 503})
            .to_string(),
        [
            json!({
                "type": "assistant", "error": "overloaded", "is_api_error_message": true,
                "message": {"content": [{"type": "text", "text": "API Error: Overloaded"}]}
            })
            .to_string(),
            json!({"type": "result", "is_error": true, "result": "Overloaded", "api_error_status": 529})
                .to_string(),
        ]
        .join("\n"),
        json!({"type": "error", "error": {"type": "overloaded_error", "status": 529, "message": "Overloaded"}})
            .to_string(),
        json!({"type": "error", "error": {"type": "server_error", "status": 502, "message": "Bad gateway"}})
            .to_string(),
        json!({"type": "result", "is_error": true, "subtype": "server_error", "result": "upstream"})
            .to_string(),
    ];
    for stream in cases {
        let infra = parse_stream(&stream).infra.expect(&stream);
        assert_eq!(infra.kind, InfraKind::Network, "{stream}");
        assert!(!infra.raw.is_empty(), "{stream}");
    }
}

#[test]
fn claude_assistant_prose_mentioning_5xx_or_overloaded_is_not_infra() {
    let prose = [
        json!({
            "type": "assistant",
            "message": {"content": [{
                "type": "text",
                "text": "Test saw HTTP 500 503 529 server_error overloaded overloaded_error authentication_error 401"
            }]}
        })
        .to_string(),
        json!({"type": "result", "is_error": true, "result": "cargo test failed: HTTP 503 overloaded server_error"})
            .to_string(),
    ]
    .join("\n");
    assert_eq!(parse_stream(&prose).infra, None);

    // Recovered run (successful result after earlier error) is not infra.
    let recovered = [
        json!({"type": "error", "error": {"type": "overloaded_error", "status": 529, "message": "Overloaded"}})
            .to_string(),
        json!({"type": "result", "is_error": false, "result": "all tests pass"}).to_string(),
    ]
    .join("\n");
    assert_eq!(parse_stream(&recovered).infra, None);
}

#[test]
fn claude_structured_auth_errors_map_to_auth() {
    let cases = [
        json!({"type": "error", "error": {"type": "authentication_error", "status": 401, "message": "invalid key"}})
            .to_string(),
        json!({"type": "result", "is_error": true, "result": "Unauthorized", "api_error_status": 401})
            .to_string(),
        [
            json!({
                "type": "assistant", "error": "authentication_error", "is_api_error_message": true,
                "message": {"content": [{"type": "text", "text": "invalid credentials"}]}
            })
            .to_string(),
            json!({"type": "result", "is_error": true, "result": "invalid credentials"}).to_string(),
        ]
        .join("\n"),
    ];
    for stream in cases {
        let infra = parse_stream(&stream).infra.expect(&stream);
        assert_eq!(infra.kind, InfraKind::Auth, "{stream}");
    }
}

#[test]
fn transient_backoff_secs_stays_within_1_to_60() {
    assert_eq!(MAX_TRANSIENT_BACKOFF_SECS, 60);
    assert_eq!(MAX_TRANSIENT_RETRIES, 3);
    for attempt in 0..=20 {
        for _ in 0..8 {
            let secs = transient_backoff_secs(attempt);
            assert!(
                (1..=MAX_TRANSIENT_BACKOFF_SECS).contains(&secs),
                "attempt={attempt} produced out-of-range backoff {secs}s"
            );
        }
    }
}

// ---- deterministic fakes for scheduler tests ------------------------------

struct FakeClock(AtomicI64);
impl FakeClock {
    fn new(start: i64) -> Self {
        Self(AtomicI64::new(start))
    }
    fn advance(&self, secs: i64) {
        self.0.fetch_add(secs, Ordering::SeqCst);
    }
}
impl Clock for FakeClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct FakeLease {
    id: String,
    leased: Arc<Mutex<HashSet<String>>>,
}
impl Drop for FakeLease {
    fn drop(&mut self) {
        self.leased.lock().unwrap().remove(&self.id);
    }
}

struct FakePool {
    accounts: Vec<AccountProfile>,
    leased: Arc<Mutex<HashSet<String>>>,
}
impl FakePool {
    fn new(names: &[&str]) -> Self {
        let accounts = names
            .iter()
            .map(|name| AccountProfile {
                id: (*name).to_string(),
                provider: ProviderKind::ClaudeCode,
                config_dir: format!("/fake/home/.claude-{name}"),
                name: (*name).to_string(),
            })
            .collect();
        Self {
            accounts,
            leased: Arc::default(),
        }
    }
    fn leased_count(&self) -> usize {
        self.leased.lock().unwrap().len()
    }
}
impl AccountPool for FakePool {
    fn acquire(&self) -> PortResult<Option<Grant>> {
        let mut leased = self.leased.lock().unwrap();
        for acc in &self.accounts {
            if !leased.contains(&acc.id) {
                leased.insert(acc.id.clone());
                let lease = FakeLease {
                    id: acc.id.clone(),
                    leased: Arc::clone(&self.leased),
                };
                return Ok(Some(Grant::new(Some(acc.clone()), Some(Box::new(lease)))));
            }
        }
        Ok(None)
    }
    fn report_limit(&self, _: &str, _: &LimitHit) -> PortResult<()> {
        Ok(())
    }
    fn profiles(&self) -> PortResult<Vec<AccountProfile>> {
        Ok(self.accounts.clone())
    }
}

#[derive(Default)]
struct FakeNotifier(Mutex<Vec<WebhookEvent>>);
impl Notifier for FakeNotifier {
    fn notify(&self, event: &WebhookEvent) -> Result<bool, String> {
        self.0.lock().unwrap().push(event.clone());
        Ok(false)
    }
}

struct FakeGit {
    base: PathBuf,
    merged: Mutex<HashSet<String>>,
}
impl FakeGit {
    fn new(base: PathBuf) -> Self {
        Self {
            base,
            merged: Mutex::default(),
        }
    }
    fn mark_merged(&self, task_id: &str) {
        self.merged.lock().unwrap().insert(task_id.to_string());
    }
}
impl GitOps for FakeGit {
    fn task_worktree(&self, task_id: &str) -> PortResult<TaskWorktree> {
        Ok(TaskWorktree {
            path: self.base.join(task_id),
            branch: format!("aaf/{task_id}"),
        })
    }
    fn commit_all(&self, _: &TaskWorktree, _: &str) -> PortResult<()> {
        Ok(())
    }
    fn has_changes(&self, _: &TaskWorktree) -> PortResult<bool> {
        Ok(true)
    }
    fn diff(&self, _: &TaskWorktree) -> PortResult<String> {
        Ok("+ok\n".to_string())
    }
    fn integrate(&self, task_ids: &[String]) -> PortResult<Integration> {
        Ok(Integration {
            tree: TaskWorktree {
                path: self.base.join("batch"),
                branch: "aaf/batch".to_string(),
            },
            merged: task_ids.to_vec(),
            conflicted: Vec::new(),
        })
    }
    fn discard(&self, _: &Integration) -> PortResult<()> {
        Ok(())
    }
    fn rebase(&self, _: &TaskWorktree) -> PortResult<Vec<String>> {
        Ok(Vec::new())
    }
    fn base_sha(&self) -> PortResult<String> {
        Ok("base0".to_string())
    }
    fn merge(
        &self,
        _: &TaskWorktree,
        _: &dyn Fn(&Path) -> Result<bool, String>,
    ) -> PortResult<MergeOutcome> {
        Ok(MergeOutcome::Merged {
            new_base_sha: "base1".to_string(),
        })
    }
    fn open_pr(
        &self,
        _: &TaskWorktree,
        _: &str,
        _: &Receipt,
        _: Option<&str>,
    ) -> PortResult<PrOutcome> {
        Ok(PrOutcome::Skipped {
            reason: "fake".to_string(),
        })
    }
    fn fetch_base(&self) -> PortResult<()> {
        Ok(())
    }
    fn is_merged(&self, task_id: &str) -> PortResult<bool> {
        Ok(self.merged.lock().unwrap().contains(task_id))
    }
    fn cleanup(&self, _: &str) -> PortResult<()> {
        Ok(())
    }
}

struct ClearGate;
impl GateRunner for ClearGate {
    fn run(&self, tree: &Path) -> PortResult<Receipt> {
        Ok(Receipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            repo_root: tree.to_string_lossy().into_owned(),
            tree_fp: "git-tree:clear".to_string(),
            head_sha: None,
            base_sha: None,
            mode: GateMode::Scoped,
            status: GateStatus::Clear,
            reasons: Vec::new(),
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

#[derive(Default)]
struct ScriptedRunner {
    outcomes: Mutex<HashMap<String, VecDeque<AgentOutcome>>>,
}
impl ScriptedRunner {
    fn script(&self, task_id: &str, outcomes: Vec<AgentOutcome>) {
        self.outcomes
            .lock()
            .unwrap()
            .insert(task_id.to_string(), outcomes.into());
    }
}
impl AgentRunner for ScriptedRunner {
    fn run(&self, request: &AgentRequest) -> PortResult<AgentOutcome> {
        let task_id = Path::new(&request.cwd)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let next = self
            .outcomes
            .lock()
            .unwrap()
            .get_mut(&task_id)
            .and_then(VecDeque::pop_front);
        Ok(next.unwrap_or_else(ok_outcome))
    }
}

fn ok_outcome() -> AgentOutcome {
    AgentOutcome {
        exit_code: Some(0),
        timed_out: false,
        final_text: "done".to_string(),
        tokens_in: 10,
        tokens_out: 5,
        limit_hit: None,
        infra: None,
        log_tail: String::new(),
    }
}

fn infra_outcome(kind: InfraKind, raw: &str) -> AgentOutcome {
    AgentOutcome {
        exit_code: Some(1),
        infra: Some(InfraFailure {
            kind,
            raw: raw.to_string(),
        }),
        log_tail: raw.to_string(),
        ..ok_outcome()
    }
}

struct TestWorld {
    tmp: TempDir,
    db: Db,
    clock: Arc<FakeClock>,
    pool: Arc<FakePool>,
    git: Arc<FakeGit>,
    runner: Arc<ScriptedRunner>,
    notifier: Arc<FakeNotifier>,
    repo: RepoConfig,
    max_parallel: u32,
    auto_merge: bool,
}

impl TestWorld {
    fn new(accounts: &[&str]) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
        Self {
            git: Arc::new(FakeGit::new(tmp.path().join("wt"))),
            tmp,
            db,
            clock: Arc::new(FakeClock::new(10_000)),
            pool: Arc::new(FakePool::new(accounts)),
            runner: Arc::default(),
            notifier: Arc::default(),
            repo: RepoConfig::default(),
            max_parallel: 1,
            auto_merge: false,
        }
    }

    fn add_ready(&self, id: &str) {
        let mut task = Task::new_root(
            id,
            REPO,
            format!("task {id}"),
            ImmutableSpec {
                description: format!("implement {id}"),
                acceptance: vec!["ok".into()],
            },
            Priority::P2,
            1,
        );
        task.state = TaskState::Ready;
        self.db.insert_task(&task).unwrap();
    }

    fn deps(&self) -> Deps {
        Deps {
            store: Arc::new(self.db.clone()),
            clock: self.clock.clone(),
            runner: self.runner.clone(),
            accounts: self.pool.clone(),
            git: self.git.clone(),
            gate: Arc::new(ClearGate),
            notifier: self.notifier.clone(),
            settings: Settings {
                repo_root: REPO.to_string(),
                repo: self.repo.clone(),
                max_parallel: self.max_parallel,
                auto_merge: self.auto_merge,
                sandboxed: false,
                hook_required: false,
                agent_path: Default::default(),
                mobile: false,
                data_dir: self.tmp.path().to_path_buf(),
                home: PathBuf::from("/fake/home"),
            },
        }
    }
}

// ---- scheduler self-healing tests -----------------------------------------

#[test]
fn auto_mode_transient_network_releases_grant_retries_and_recovers() {
    let world = TestWorld::new(&["acct-a"]);
    world.add_ready("t1");
    world.runner.script(
        "t1",
        vec![
            infra_outcome(InfraKind::Network, "HTTP 503 service unavailable"),
            infra_outcome(InfraKind::Network, "HTTP 529 overloaded"),
            ok_outcome(),
        ],
    );

    let ticks = AtomicU32::new(0);
    let hooks = Hooks {
        idle: &|| {
            // Lease MUST be released while waiting out transient backoff.
            assert_eq!(world.pool.leased_count(), 0);
            let done = world.db.get_task("t1").unwrap().state == TaskState::Review;
            if done || ticks.fetch_add(1, Ordering::SeqCst) >= 5 {
                return false;
            }
            world.clock.advance(MAX_TRANSIENT_BACKOFF_SECS as i64 + 5);
            true
        },
        reap: &|_| Ok(()),
    };

    let summary = run(&world.deps(), Mode::Auto, &hooks).unwrap();
    assert_eq!(summary.halted, None, "{summary:?}");
    assert_eq!(summary.agent_runs, 3);
    assert_eq!(summary.review, vec!["t1"]);
    assert!(summary.requeued.is_empty());
    assert_eq!(world.db.get_task("t1").unwrap().state, TaskState::Review);
    assert_eq!(world.pool.leased_count(), 0);
}

/// O12-R10-1: an outage longer than the quick retries does not stop an
/// unattended daemon: it waits with a growing pause (one notification), keeps
/// the task Ready, and picks it up again once the network is back.
#[test]
fn auto_mode_transient_network_waits_out_a_long_outage_and_recovers() {
    let world = TestWorld::new(&["acct-a"]);
    world.add_ready("t1");
    let mut outcomes: Vec<_> = (0..6)
        .map(|i| infra_outcome(InfraKind::Network, &format!("HTTP 503 attempt {i}")))
        .collect();
    outcomes.push(ok_outcome());
    world.runner.script("t1", outcomes);

    let ticks = AtomicU32::new(0);
    let hooks = Hooks {
        idle: &|| {
            assert_eq!(world.pool.leased_count(), 0);
            let done = world.db.get_task("t1").unwrap().state == TaskState::Review;
            if done || ticks.fetch_add(1, Ordering::SeqCst) >= 400 {
                return false;
            }
            world.clock.advance(30);
            true
        },
        reap: &|_| Ok(()),
    };

    let summary = run(&world.deps(), Mode::Auto, &hooks).unwrap();
    assert_eq!(summary.halted, None, "{summary:?}");
    assert_eq!(summary.agent_runs, 7, "{summary:?}");
    assert_eq!(world.db.get_task("t1").unwrap().state, TaskState::Review);
    assert_eq!(world.pool.leased_count(), 0);
    let notified = world.notifier.0.lock().unwrap();
    assert_eq!(notified.len(), 1, "one notification per outage");
    assert_eq!(notified[0].task_id.as_deref(), Some("t1"));
    // The failures after the MAX_TRANSIENT_RETRIES (3) quick retries wait
    // 300 + 600 + 1200 s, at least.
    assert!(
        world.clock.now() >= 10_000 + 300 + 600 + 1200,
        "{}",
        world.clock.now()
    );
}

#[test]
fn auto_mode_auth_failure_halts_immediately_without_retrying() {
    let world = TestWorld::new(&["acct-a"]);
    world.add_ready("t1");
    world.add_ready("t2");
    world.runner.script(
        "t1",
        vec![infra_outcome(InfraKind::Auth, "401 authentication_error")],
    );

    let idle_calls = AtomicU32::new(0);
    let hooks = Hooks {
        idle: &|| {
            idle_calls.fetch_add(1, Ordering::SeqCst);
            world.clock.advance(65);
            true
        },
        reap: &|_| Ok(()),
    };

    let summary = run(&world.deps(), Mode::Auto, &hooks).unwrap();
    assert!(summary.halted.is_some(), "{summary:?}");
    assert_eq!(summary.agent_runs, 1);
    assert_eq!(idle_calls.load(Ordering::SeqCst), 0);
    assert_eq!(world.db.get_task("t1").unwrap().state, TaskState::Ready);
    assert_eq!(world.db.get_task("t2").unwrap().state, TaskState::Ready);
}

// ---- queue brake (`max_open_review`) test ---------------------------------

#[test]
fn queue_brake_caps_open_review_and_resumes_after_sweep_merge() {
    let mut world = TestWorld::new(&["acct-a", "acct-b"]);
    world.max_parallel = 2;
    world.auto_merge = false;
    world.repo.run.max_open_review = 2;
    for id in ["t1", "t2", "t3"] {
        world.add_ready(id);
    }

    let hooks_once = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };
    let first = run(&world.deps(), Mode::Once, &hooks_once).unwrap();
    assert_eq!(first.review.len(), 2, "{first:?}");
    assert_eq!(first.agent_runs, 2);
    assert!(first.waiting_for_review_cap > 0, "{first:?}");
    assert_eq!(first.waiting_for_account, 0, "{first:?}");
    assert_eq!(world.db.get_task("t3").unwrap().state, TaskState::Ready);

    // Mark t1 merged: next run sweeps t1 -> Done, freeing a review slot for t3.
    world.git.mark_merged("t1");
    let second = run(&world.deps(), Mode::Once, &hooks_once).unwrap();
    assert_eq!(second.done, vec!["t1"]);
    assert_eq!(second.review, vec!["t3"]);
    assert_eq!(second.waiting_for_account, 0);
    assert_eq!(world.db.get_task("t1").unwrap().state, TaskState::Done);
    assert_eq!(world.db.get_task("t3").unwrap().state, TaskState::Review);
}
