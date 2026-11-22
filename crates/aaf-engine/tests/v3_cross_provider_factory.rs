#![allow(clippy::unwrap_used, clippy::expect_used)]
//! End-to-end factory cross-provider audit tests (`run_with_audit`,
//! `AuditBackend`, `require_different_provider` fail-closed).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::blocked::BlockedReason;
use aaf_domain::config::RepoConfig;
use aaf_domain::event::EventKind;
use aaf_domain::gate::{
    GateMode, GateStatus, HeadLockReport, LockStatus, RECEIPT_SCHEMA_VERSION, Receipt,
    StoreComplianceReport,
};
use aaf_domain::ports::{
    AgentOutcome, AgentRequest, AgentRunner, Clock, EventSink, LimitHit, PortResult, TaskStore,
};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::cross_audit::build_checklist;
use aaf_engine::db::Db;
use aaf_engine::factory::{
    AccountPool, AuditBackend, Deps, GateRunner, GitOps, Grant, Hooks, Integration, Mode, Notifier,
    Settings, run_with_audit,
};
use aaf_engine::git::{MergeOutcome, PrOutcome, TaskWorktree};
use aaf_engine::webhook::WebhookEvent;
use tempfile::TempDir;

struct TestClock(AtomicI64);
impl Clock for TestClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct TestPool(Vec<AccountProfile>);
impl AccountPool for TestPool {
    fn acquire(&self) -> PortResult<Option<Grant>> {
        Ok(Some(Grant::new(self.0.first().cloned(), None)))
    }
    fn report_limit(&self, _: &str, _: &LimitHit) -> PortResult<()> {
        Ok(())
    }
    fn profiles(&self) -> PortResult<Vec<AccountProfile>> {
        Ok(self.0.clone())
    }
}

struct TestNotifier;
impl Notifier for TestNotifier {
    fn notify(&self, _: &WebhookEvent) -> Result<bool, String> {
        Ok(false)
    }
}

#[derive(Default)]
struct TestRunner {
    final_text: String,
    exit_code: Option<i32>,
    log_tail: String,
    requests: Mutex<Vec<AgentRequest>>,
}

impl TestRunner {
    fn ok(final_text: impl Into<String>) -> Self {
        Self {
            final_text: final_text.into(),
            exit_code: Some(0),
            log_tail: String::new(),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn fail(log_tail: impl Into<String>) -> Self {
        Self {
            final_text: String::new(),
            exit_code: Some(1),
            log_tail: log_tail.into(),
            requests: Mutex::new(Vec::new()),
        }
    }
}

impl AgentRunner for TestRunner {
    fn run(&self, request: &AgentRequest) -> PortResult<AgentOutcome> {
        self.requests.lock().unwrap().push(request.clone());
        Ok(AgentOutcome {
            exit_code: self.exit_code,
            timed_out: false,
            final_text: self.final_text.clone(),
            tokens_in: 12,
            tokens_out: 8,
            limit_hit: None,
            infra: None,
            log_tail: self.log_tail.clone(),
        })
    }
}

struct TestGit {
    base: PathBuf,
    counter: AtomicI64,
}

impl GitOps for TestGit {
    fn task_worktree(&self, task_id: &str) -> PortResult<TaskWorktree> {
        Ok(TaskWorktree {
            path: self.base.join(format!("wt-{task_id}")),
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
        Ok("+modified locked file\n".to_string())
    }
    fn integrate(&self, _: &[String]) -> PortResult<Integration> {
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        Ok(Integration {
            tree: TaskWorktree {
                path: self.base.join(format!("clean-{n}")),
                branch: format!("aaf/clean-{n}"),
            },
            merged: Vec::new(),
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
            reason: "test".to_string(),
        })
    }
    fn fetch_base(&self) -> PortResult<()> {
        Ok(())
    }
    fn is_merged(&self, _: &str) -> PortResult<bool> {
        Ok(false)
    }
    fn cleanup(&self, _: &str) -> PortResult<()> {
        Ok(())
    }
}

struct LockedGate {
    data_dir: PathBuf,
    locks: HashMap<String, String>,
}

impl GateRunner for LockedGate {
    fn run(&self, tree: &Path) -> PortResult<Receipt> {
        let name = tree.file_name().unwrap().to_string_lossy().to_string();
        let blocked = self
            .locks
            .get(&name)
            .is_some_and(|fp| !aaf_fp::approval_path(&self.data_dir, tree, fp).is_file());
        Ok(Receipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            repo_root: tree.to_string_lossy().into_owned(),
            tree_fp: format!("git-tree:{name}"),
            head_sha: None,
            base_sha: None,
            mode: GateMode::Scoped,
            status: if blocked {
                GateStatus::Blocked
            } else {
                GateStatus::Clear
            },
            reasons: if blocked {
                vec!["head lock".to_string()]
            } else {
                Vec::new()
            },
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
            created_at: 1000,
        })
    }

    fn head_lock(&self, tree: &Path) -> PortResult<HeadLockReport> {
        let name = tree.file_name().unwrap().to_string_lossy().to_string();
        let (status, lock_fp) = match self.locks.get(&name) {
            None => (LockStatus::Clear, String::new()),
            Some(fp) if aaf_fp::approval_path(&self.data_dir, tree, fp).is_file() => {
                (LockStatus::Approved, fp.clone())
            }
            Some(fp) => (LockStatus::Blocked, fp.clone()),
        };
        Ok(HeadLockReport {
            status,
            lock_fp,
            locked_changes: Vec::new(),
            approval: None,
        })
    }
}

fn pass_audit_json(spec: &ImmutableSpec) -> String {
    let answers: Vec<serde_json::Value> = build_checklist(spec, false)
        .iter()
        .map(|item| serde_json::json!({ "id": item.id, "answer": "no" }))
        .collect();
    serde_json::json!({ "answers": answers }).to_string()
}

fn setup_factory(
    require_different_provider: bool,
) -> (TempDir, Db, Arc<TestRunner>, Deps, ImmutableSpec) {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    let spec = ImmutableSpec {
        description: "Update locked test cleanly".to_string(),
        acceptance: vec!["all tests pass".to_string()],
    };
    let mut task = Task::new_root("t1", "/fake/repo", "Task t1", spec.clone(), Priority::P2, 1);
    task.state = TaskState::Ready;
    db.insert_task(&task).unwrap();

    let author_runner = Arc::new(TestRunner::ok("implemented"));
    let mut repo_cfg = RepoConfig::default();
    repo_cfg.run.executor = "claude".to_string();
    repo_cfg.cross_audit.require_different_provider = require_different_provider;

    let mut locks = HashMap::new();
    locks.insert("wt-t1".to_string(), "lockfp-v3".to_string());

    let deps = Deps {
        store: Arc::new(db.clone()),
        clock: Arc::new(TestClock(AtomicI64::new(2_000))),
        runner: author_runner.clone(),
        accounts: Arc::new(TestPool(vec![AccountProfile {
            id: "claude-a".to_string(),
            provider: ProviderKind::ClaudeCode,
            config_dir: tmp.path().join(".claude-a").to_string_lossy().into_owned(),
            name: "a".to_string(),
        }])),
        git: Arc::new(TestGit {
            base: tmp.path().to_path_buf(),
            counter: AtomicI64::new(0),
        }),
        gate: Arc::new(LockedGate {
            data_dir: tmp.path().to_path_buf(),
            locks,
        }),
        notifier: Arc::new(TestNotifier),
        settings: Settings {
            repo_root: "/fake/repo".to_string(),
            repo: repo_cfg,
            max_parallel: 1,
            auto_merge: false,
            sandboxed: true,
            hook_required: false,
            agent_path: Default::default(),
            mobile: false,
            data_dir: tmp.path().to_path_buf(),
            home: tmp.path().to_path_buf(),
        },
    };
    (tmp, db, author_runner, deps, spec)
}

#[test]
fn cross_provider_audit_writes_independent_approval_in_clean_sandboxed_tree() {
    let (tmp, db, author_runner, deps, spec) = setup_factory(true);
    let audit_runner = Arc::new(TestRunner::ok(pass_audit_json(&spec)));
    let audit = AuditBackend {
        provider: "gemini".to_string(),
        runner: audit_runner.clone(),
        accounts: Arc::new(TestPool(Vec::new())),
        hook_required: false,
    };
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };

    let summary = run_with_audit(&deps, Some(&audit), Mode::Once, &hooks).unwrap();
    assert_eq!(summary.review, vec!["t1"]);
    assert!(summary.blocked.is_empty(), "{summary:?}");
    assert_eq!(author_runner.requests.lock().unwrap().len(), 1);

    let audit_reqs = audit_runner.requests.lock().unwrap();
    assert_eq!(audit_reqs.len(), 1);
    assert!(
        audit_reqs[0].cwd.ends_with("clean-0"),
        "{}",
        audit_reqs[0].cwd
    );
    assert!(
        audit_reqs[0].sandbox.enabled,
        "auditor must remain sandboxed"
    );

    let key = aaf_fp::load_key(tmp.path()).unwrap();
    let wt = tmp.path().join("wt-t1");
    let approval = aaf_fp::read_approval(tmp.path(), &key, &wt, "lockfp-v3")
        .unwrap()
        .expect("signed approval written");
    assert_eq!(approval.reviewer, "cross-audit:gemini/default");
    assert!(!approval.same_account);

    let events = db.events_for_task("t1", 50).unwrap();
    let ca = events
        .iter()
        .find(|e| e.kind == EventKind::CrossAudit)
        .expect("cross_audit event emitted");
    assert!(
        ca.payload_json.contains("\"same_account\":false"),
        "{}",
        ca.payload_json
    );
    assert!(
        ca.payload_json.contains("\"provider\":\"gemini\""),
        "{}",
        ca.payload_json
    );
}

#[test]
fn require_different_provider_fails_closed_without_writing_approval() {
    let (tmp, db, author_runner, deps, _spec) = setup_factory(true);
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };

    let summary = run_with_audit(&deps, None, Mode::Once, &hooks).unwrap();
    assert_eq!(summary.blocked, vec!["t1"]);
    assert!(summary.review.is_empty(), "{summary:?}");
    assert_eq!(author_runner.requests.lock().unwrap().len(), 1);

    let task = db.get_task("t1").unwrap();
    assert_eq!(task.state, TaskState::Blocked);
    match task.blocked_reason {
        Some(BlockedReason::NeedsHuman { why }) => {
            assert!(why.contains("require_different_provider"), "{why}");
        }
        other => panic!("expected NeedsHuman, got {other:?}"),
    }
    let approval_file = aaf_fp::approval_path(tmp.path(), &tmp.path().join("wt-t1"), "lockfp-v3");
    assert!(!approval_file.exists(), "no approval may be written");
}

#[test]
fn non_claude_auditor_http_401_and_429_route_to_halt_or_cooldown_not_task_failure() {
    // HTTP 401 from non-Claude auditor -> Halt, task remains Gating with 0 unblock cycles.
    let (_tmp, db, _author, deps, _spec) = setup_factory(false);
    let audit_auth = AuditBackend {
        provider: "codex".to_string(),
        runner: Arc::new(TestRunner::fail("Error: HTTP 401 Unauthorized")),
        accounts: Arc::new(TestPool(Vec::new())),
        hook_required: false,
    };
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };
    let summary = run_with_audit(&deps, Some(&audit_auth), Mode::Once, &hooks).unwrap();
    assert!(summary.halted.is_some(), "{summary:?}");
    let task = db.get_task("t1").unwrap();
    assert_eq!(task.state, TaskState::Gating);
    assert_eq!(task.unblock_cycles, 0);

    // HTTP 429 / rate limit from non-Claude auditor -> Wait/cooldown, task stays Gating.
    let (_tmp2, db2, _author2, deps2, _spec2) = setup_factory(false);
    let audit_429 = AuditBackend {
        provider: "gemini".to_string(),
        runner: Arc::new(TestRunner::fail("HTTP 429 rate limit exceeded")),
        accounts: Arc::new(TestPool(Vec::new())),
        hook_required: false,
    };
    let summary2 = run_with_audit(&deps2, Some(&audit_429), Mode::Once, &hooks).unwrap();
    assert!(summary2.halted.is_none(), "{summary2:?}");
    assert!(summary2.blocked.is_empty(), "{summary2:?}");
    let task2 = db2.get_task("t1").unwrap();
    assert_eq!(task2.state, TaskState::Gating);
    assert_eq!(task2.unblock_cycles, 0);
}

#[test]
fn audit_rejection_hands_off_to_scheduler_unblock_with_real_work_grant() {
    let (_tmp, db, author_runner, deps, _spec) = setup_factory(false);
    // Trigger audit rejection (inconclusive non-JSON output) twice so second hits T4 proposer.
    let audit_fail = AuditBackend {
        provider: "gemini".to_string(),
        runner: Arc::new(TestRunner::ok("not valid audit json")),
        accounts: Arc::new(TestPool(vec![AccountProfile {
            id: "gemini-auditor".into(),
            provider: ProviderKind::Antigravity,
            config_dir: "/tmp/g".into(),
            name: "g".into(),
        }])),
        hook_required: false,
    };
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };
    // Run 1: audit rejects -> Next::Unblock -> T3 fresh context -> Ready.
    let s1 = run_with_audit(&deps, Some(&audit_fail), Mode::Once, &hooks).unwrap();
    assert_eq!(s1.requeued, vec!["t1"]);
    assert_eq!(db.get_task("t1").unwrap().state, TaskState::Ready);

    // Run 2: audit rejects same digest -> Next::Unblock -> T4 proposer runs on real Work grant (`claude-a`).
    let s2 = run_with_audit(&deps, Some(&audit_fail), Mode::Once, &hooks).unwrap();
    assert_eq!(s2.blocked, vec!["t1"]);
    let reqs = author_runner.requests.lock().unwrap();
    let proposer_req = reqs
        .iter()
        .find(|r| r.prompt.contains("Split it into"))
        .expect("proposer must run on author runner");
    assert_eq!(
        proposer_req.account.as_ref().map(|a| a.id.as_str()),
        Some("claude-a"),
        "unblock proposer must use real Purpose::Work grant, never Grant::new(None, None)"
    );
}

#[test]
fn three_split_subtasks_auto_complete_parent_without_no_change_or_freeze() {
    let (_tmp, db, author_runner, mut deps, spec) = setup_factory(false);
    deps.settings.auto_merge = true;
    deps.gate = Arc::new(LockedGate {
        data_dir: deps.settings.data_dir.clone(),
        locks: HashMap::new(),
    });
    let parent = db.get_task("t1").unwrap();
    let plans = vec![
        aaf_engine::split::SplitSubtask {
            key: "a".into(),
            title: "Part A".into(),
            description: "Do A".into(),
            acceptance: vec!["a ok".into()],
            owned_paths: vec!["src/a.rs".into()],
            depends_on: vec![],
        },
        aaf_engine::split::SplitSubtask {
            key: "b".into(),
            title: "Part B".into(),
            description: "Do B".into(),
            acceptance: vec!["b ok".into()],
            owned_paths: vec!["src/b.rs".into()],
            depends_on: vec![],
        },
        aaf_engine::split::SplitSubtask {
            key: "c".into(),
            title: "Part C".into(),
            description: "Do C".into(),
            acceptance: vec!["c ok".into()],
            owned_paths: vec!["src/c.rs".into()],
            depends_on: vec![],
        },
    ];
    let _ = spec;
    let outcome = aaf_engine::split::apply_split(
        &db,
        &parent,
        &plans,
        &deps.settings.repo.unblocker,
        10,
        false,
    )
    .unwrap();
    assert_eq!(outcome.subtasks.len(), 3);

    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };
    let summary = run_with_audit(&deps, None, Mode::Once, &hooks).unwrap();
    assert!(!summary.frozen);
    assert!(summary.blocked.is_empty(), "{summary:?}");
    assert_eq!(
        summary.agent_runs, 3,
        "parent must not run a 4th NO_CHANGE agent"
    );
    assert_eq!(author_runner.requests.lock().unwrap().len(), 3);
    assert_eq!(db.get_task("t1").unwrap().state, TaskState::Done);
    assert_eq!(db.get_task("t1").unwrap().unblock_cycles, 0);
    for sub in &outcome.subtasks {
        assert_eq!(db.get_task(&sub.id).unwrap().state, TaskState::Done);
    }
}
