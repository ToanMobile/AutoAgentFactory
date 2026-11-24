#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for Round 4A Fixer 3:
//! - `Db::apply_split_atomic` inheriting live in-tx `parent_state`
//! - `audit_turn`: read-only sandbox, fresh `repair_req.run_id`, fail-closed guards
//! - `scheduler`/`sweep`: `expire_waits()` unparking + review-cap `sweep_review` throttle
//! - `post_audit`: mechanical score short-circuit with 0 LLM / integrate calls

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::config::{AuditScope, RepoConfig, UnblockerConfig};
use aaf_domain::event::EventKind;
use aaf_domain::gate::{
    GateMode, GateStatus, HeadLockReport, LockStatus, RECEIPT_SCHEMA_VERSION, Receipt, Severity,
    StaticFinding, StoreComplianceReport, StoreStatus,
};
use aaf_domain::ports::{
    AgentOutcome, AgentRequest, AgentRunner, BudgetStore, Clock, EventSink, LimitHit, PortResult,
    TaskStore,
};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::cross_audit::build_checklist;
use aaf_engine::cross_audit::prompt::{MAX_REPAIR_PRIOR_CHARS, build_format_repair_prompt};
use aaf_engine::db::Db;
use aaf_engine::factory::{
    AccountPool, AuditBackend, Deps, GateRunner, GitOps, Grant, Hooks, Integration, Mode, Notifier,
    Settings, run, run_with_audit,
};
use aaf_engine::git::{MergeOutcome, PrOutcome, TaskWorktree};
use aaf_engine::split::{SplitSubtask, apply_split};
use aaf_engine::unblocker::QuotaRoute;
use aaf_engine::webhook::WebhookEvent;
use tempfile::TempDir;

struct TestClock(AtomicI64);
impl Clock for TestClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct NullNotifier;
impl Notifier for NullNotifier {
    fn notify(&self, _: &WebhookEvent) -> Result<bool, String> {
        Ok(false)
    }
}

struct WaitPool(Vec<AccountProfile>);
impl AccountPool for WaitPool {
    fn acquire(&self) -> PortResult<Option<Grant>> {
        Ok(Some(Grant::new(self.0.first().cloned(), None)))
    }
    fn after_limit(&self) -> PortResult<Option<QuotaRoute>> {
        Ok(Some(QuotaRoute::Wait))
    }
    fn report_limit(&self, _: &str, _: &LimitHit) -> PortResult<()> {
        Ok(())
    }
    fn profiles(&self) -> PortResult<Vec<AccountProfile>> {
        Ok(self.0.clone())
    }
}

struct ScriptedRunner(Mutex<VecDeque<AgentOutcome>>, Mutex<Vec<AgentRequest>>);
impl ScriptedRunner {
    fn from_texts(texts: &[&str]) -> Arc<Self> {
        Arc::new(Self(
            Mutex::new(texts.iter().map(|s| ok_outcome(s)).collect()),
            Mutex::new(Vec::new()),
        ))
    }
}
impl AgentRunner for ScriptedRunner {
    fn run(&self, req: &AgentRequest) -> PortResult<AgentOutcome> {
        self.1.lock().unwrap().push(req.clone());
        Ok(self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| ok_outcome("done")))
    }
}

fn ok_outcome(text: &str) -> AgentOutcome {
    AgentOutcome {
        exit_code: Some(0),
        timed_out: false,
        final_text: text.into(),
        tokens_in: 40,
        tokens_out: 20,
        limit_hit: None,
        infra: None,
        log_tail: String::new(),
    }
}

fn spec() -> ImmutableSpec {
    ImmutableSpec {
        description: "Verify round 4 factory fixes".into(),
        acceptance: vec!["all invariants hold".into()],
    }
}

fn pass_json() -> String {
    let ans: Vec<_> = build_checklist(&spec(), false)
        .iter()
        .map(|i| serde_json::json!({ "id": i.id, "answer": "no" }))
        .collect();
    serde_json::json!({ "answers": ans }).to_string()
}

struct FakeGit {
    base: PathBuf,
    integrates: AtomicUsize,
    fetches: AtomicUsize,
    is_merged_calls: AtomicUsize,
}
impl FakeGit {
    fn new(base: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            base,
            integrates: AtomicUsize::new(0),
            fetches: AtomicUsize::new(0),
            is_merged_calls: AtomicUsize::new(0),
        })
    }
}
impl GitOps for FakeGit {
    fn task_worktree(&self, id: &str) -> PortResult<TaskWorktree> {
        let path = self.base.join(format!("wt-{id}"));
        std::fs::create_dir_all(&path).unwrap();
        Ok(TaskWorktree {
            path,
            branch: format!("aaf/{id}"),
        })
    }
    fn commit_all(&self, _: &TaskWorktree, _: &str) -> PortResult<()> {
        Ok(())
    }
    fn has_changes(&self, _: &TaskWorktree) -> PortResult<bool> {
        Ok(true)
    }
    fn diff(&self, _: &TaskWorktree) -> PortResult<String> {
        Ok("diff --git a/src/lib.rs b/src/lib.rs\n+pub fn ok() {}\n".into())
    }
    fn integrate(&self, ids: &[String]) -> PortResult<Integration> {
        let n = self.integrates.fetch_add(1, Ordering::SeqCst);
        let path = self.base.join(format!("clean-{n}"));
        std::fs::create_dir_all(&path).unwrap();
        Ok(Integration {
            tree: TaskWorktree {
                path,
                branch: format!("aaf/clean-{n}"),
            },
            merged: ids.to_vec(),
            conflicted: vec![],
        })
    }
    fn discard(&self, _: &Integration) -> PortResult<()> {
        Ok(())
    }
    fn rebase(&self, _: &TaskWorktree) -> PortResult<Vec<String>> {
        Ok(vec![])
    }
    fn base_sha(&self) -> PortResult<String> {
        Ok("base0".into())
    }
    fn merge(
        &self,
        _: &TaskWorktree,
        _: &dyn Fn(&Path) -> Result<bool, String>,
    ) -> PortResult<MergeOutcome> {
        Ok(MergeOutcome::Merged {
            new_base_sha: "base1".into(),
        })
    }
    fn open_pr(
        &self,
        _: &TaskWorktree,
        _: &str,
        _: &Receipt,
        _: Option<&str>,
    ) -> PortResult<PrOutcome> {
        Ok(PrOutcome::Opened {
            url: "https://example.test/pr/1".into(),
        })
    }
    fn fetch_base(&self) -> PortResult<()> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn is_merged(&self, _: &str) -> PortResult<bool> {
        self.is_merged_calls.fetch_add(1, Ordering::SeqCst);
        Ok(false)
    }
    fn cleanup(&self, _: &str) -> PortResult<()> {
        Ok(())
    }
}

struct StaticGate {
    cfg_hash: String,
    findings: Vec<StaticFinding>,
    unresolved: Vec<String>,
}
impl GateRunner for StaticGate {
    fn run(&self, tree: &Path) -> PortResult<Receipt> {
        let name = tree.file_name().unwrap().to_string_lossy();
        let store_compliance = if self.unresolved.is_empty() {
            StoreComplianceReport::not_applicable()
        } else {
            StoreComplianceReport {
                status: StoreStatus::Clear,
                critical: 0,
                warnings: 0,
                unresolved: self.unresolved.clone(),
                findings: vec![],
            }
        };
        Ok(Receipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            repo_root: tree.to_string_lossy().into_owned(),
            tree_fp: format!("git-tree:{name}"),
            head_sha: Some("head1".into()),
            base_sha: Some("base0".into()),
            mode: GateMode::Scoped,
            status: GateStatus::Clear,
            reasons: vec![],
            config_hash: self.cfg_hash.clone(),
            commands: vec![],
            static_findings: self.findings.clone(),
            suppressions: 0,
            head_lock: self.head_lock(tree)?,
            store_compliance,
            created_at: 2000,
        })
    }
    fn head_lock(&self, _: &Path) -> PortResult<HeadLockReport> {
        Ok(HeadLockReport {
            status: LockStatus::Clear,
            lock_fp: String::new(),
            locked_changes: vec![],
            approval: None,
        })
    }
}

struct TestEnv {
    tmp: TempDir,
    db: Db,
    clock: Arc<TestClock>,
    git: Arc<FakeGit>,
}
impl TestEnv {
    fn new(now: i64) -> Self {
        let tmp = TempDir::new().unwrap();
        let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
        let git = FakeGit::new(tmp.path().to_path_buf());
        Self {
            tmp,
            db,
            clock: Arc::new(TestClock(AtomicI64::new(now))),
            git,
        }
    }
    fn add_task(&self, id: &str, state: TaskState) {
        let mut t = Task::new_root(
            id,
            "/fake/repo",
            format!("Task {id}"),
            spec(),
            Priority::P1,
            1,
        );
        t.state = state;
        self.db.insert_task(&t).unwrap();
    }
    fn deps(
        &self,
        runner: Arc<dyn AgentRunner>,
        gate: Arc<dyn GateRunner>,
        accounts: Vec<AccountProfile>,
        repo: RepoConfig,
        max_parallel: u32,
    ) -> Deps {
        Deps {
            store: Arc::new(self.db.clone()),
            clock: self.clock.clone(),
            runner,
            accounts: Arc::new(WaitPool(accounts)),
            git: self.git.clone(),
            gate,
            notifier: Arc::new(NullNotifier),
            settings: Settings {
                repo_root: "/fake/repo".into(),
                repo,
                max_parallel,
                auto_merge: false,
                sandboxed: false,
                hook_required: false,
                agent_path: Default::default(),
                mobile: false,
                data_dir: self.tmp.path().to_path_buf(),
                home: self.tmp.path().to_path_buf(),
            },
        }
    }
}

fn hooks_stop() -> Hooks<'static> {
    Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    }
}

fn clear_gate(repo: &RepoConfig) -> Arc<StaticGate> {
    Arc::new(StaticGate {
        cfg_hash: aaf_engine::gate::config_hash(repo),
        findings: vec![],
        unresolved: vec![],
    })
}

fn mk_audit(provider: &str, runner: Arc<dyn AgentRunner>) -> AuditBackend {
    AuditBackend {
        provider: provider.into(),
        runner,
        accounts: Arc::new(WaitPool(vec![])),
        hook_required: false,
    }
}

#[test]
fn apply_split_atomic_inherits_live_ready_state_over_stale_backlog_snapshot() {
    let env = TestEnv::new(10);
    env.add_task("p1", TaskState::Backlog);
    let stale_backlog = env.db.get_task("p1").unwrap();
    env.db.set_state("p1", TaskState::Ready, None, 15).unwrap();
    let plan = |k: &str, f: &str| SplitSubtask {
        key: k.into(),
        title: k.into(),
        description: k.into(),
        acceptance: vec![k.into()],
        owned_paths: vec![f.into()],
        depends_on: vec![],
    };
    let plans = [plan("a", "src/a.rs"), plan("b", "src/b.rs")];
    let out = apply_split(
        &env.db,
        &stale_backlog,
        &plans,
        &UnblockerConfig::default(),
        20,
        false,
    )
    .unwrap();
    assert!(out.split);
    assert_eq!(env.db.get_task("p1-s1").unwrap().state, TaskState::Ready);
    assert_eq!(env.db.get_task("p1-s2").unwrap().state, TaskState::Ready);
}

#[test]
fn audit_turn_read_only_fresh_repair_id_and_fail_closed_repair_guards() {
    let prompt = build_format_repair_prompt(&build_checklist(&spec(), false), false, "bad", "x");
    assert!(prompt.contains(
        "For any checklist item not clearly answered \"no\" in your previous response, answer \"yes\"."
    ));
    let mut repo = RepoConfig::default();
    repo.cross_audit.audit_scope = AuditScope::Always;
    let pass = pass_json();
    let too_long = "x".repeat(MAX_REPAIR_PRIOR_CHARS + 1);
    for (label, first_reply, expect_repair) in [
        ("short_prose", "not json yet", true),
        ("empty_whitespace", "   \n\t ", false),
        ("exceeds_max_chars", too_long.as_str(), false),
    ] {
        let env = TestEnv::new(2000);
        env.add_task("t1", TaskState::Ready);
        let deps = env.deps(
            ScriptedRunner::from_texts(&["implemented"]),
            clear_gate(&repo),
            vec![],
            repo.clone(),
            1,
        );
        let ar = ScriptedRunner::from_texts(&[first_reply, &pass]);
        let audit = mk_audit("codex", ar.clone());
        let s = run_with_audit(&deps, Some(&audit), Mode::Once, &hooks_stop()).unwrap();
        let reqs = ar.1.lock().unwrap();
        assert!(reqs[0].sandbox.read_only, "{label}: read-only auditor");
        if expect_repair {
            assert_eq!(reqs.len(), 2, "{label}: 1-shot repair");
            assert!(reqs[1].sandbox.read_only, "{label}: read-only repair");
            assert_ne!(reqs[0].run_id, reqs[1].run_id, "{label}: fresh run_id");
            assert_eq!(s.review, vec!["t1"], "{label}: {s:?}");
        } else {
            assert_eq!(reqs.len(), 1, "{label}: fail closed without retry");
            assert!(
                s.review.is_empty() && s.requeued == vec!["t1"],
                "{label}: {s:?}"
            );
        }
    }
}

struct SiblingCooldownRunner {
    db: Db,
    clock: Arc<TestClock>,
    t1_attempts: AtomicUsize,
    t1_limited: AtomicBool,
    reset_at: i64,
}
impl AgentRunner for SiblingCooldownRunner {
    fn run(&self, req: &AgentRequest) -> PortResult<AgentOutcome> {
        if req.cwd.ends_with("wt-t1") {
            if self.t1_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                self.t1_limited.store(true, Ordering::SeqCst);
                let mut out = ok_outcome("");
                out.exit_code = Some(1);
                out.limit_hit = Some(LimitHit {
                    reset_at: Some(self.reset_at),
                    raw: "429 rate limit".into(),
                });
                return Ok(out);
            }
            return Ok(ok_outcome("t1 ok"));
        }
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            if self.t1_limited.load(Ordering::SeqCst)
                && self.db.get_task("t1").unwrap().state == TaskState::Ready
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(60));
        self.clock.0.store(self.reset_at + 20, Ordering::SeqCst);
        Ok(ok_outcome("t2 ok"))
    }
}

#[test]
fn scheduler_auto_unparks_rate_limited_task_when_sibling_finishes_after_cooldown() {
    for with_account in [false, true] {
        let env = TestEnv::new(1000);
        env.add_task("t1", TaskState::Ready);
        env.add_task("t2", TaskState::Ready);
        let runner = Arc::new(SiblingCooldownRunner {
            db: env.db.clone(),
            clock: env.clock.clone(),
            t1_attempts: AtomicUsize::new(0),
            t1_limited: AtomicBool::new(false),
            reset_at: 1010,
        });
        let accounts = if with_account {
            vec![AccountProfile {
                id: "acct-1".into(),
                provider: ProviderKind::ClaudeCode,
                config_dir: env.tmp.path().join("a1").to_string_lossy().into_owned(),
                name: "acct-1".into(),
            }]
        } else {
            vec![]
        };
        let repo = RepoConfig::default();
        let deps = env.deps(runner.clone(), clear_gate(&repo), accounts, repo, 2);
        let s = run(&deps, Mode::Auto, &hooks_stop()).unwrap();
        let mut reviewed = s.review.clone();
        reviewed.sort();
        assert_eq!(
            reviewed,
            vec!["t1", "t2"],
            "with_account={with_account}: {s:?}"
        );
        assert_eq!(runner.t1_attempts.load(Ordering::SeqCst), 2);
    }
}

#[test]
fn review_cap_throttles_redundant_sweep_review_calls() {
    let mut repo = RepoConfig::default();
    repo.run.max_open_review = 1;
    let env = TestEnv::new(1000);
    env.add_task("t1", TaskState::Review);
    env.add_task("t2", TaskState::Ready);
    let deps = env.deps(
        ScriptedRunner::from_texts(&[]),
        clear_gate(&repo),
        vec![],
        repo,
        1,
    );
    let s = run(&deps, Mode::Once, &hooks_stop()).unwrap();
    assert!(s.waiting_for_review_cap > 0);
    assert_eq!(env.git.fetches.load(Ordering::SeqCst), 1);
    assert_eq!(env.git.is_merged_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn post_audit_short_circuits_low_mechanical_score_with_zero_llm_calls() {
    let mut repo = RepoConfig::default();
    repo.cross_audit.audit_scope = AuditScope::Always;
    repo.cross_audit.score_gate = true;
    repo.cross_audit.min_average_score = 95;
    repo.cross_audit.min_criterion_score = 90;
    repo.pipeline.max_audit_reworks = 1;

    let env = TestEnv::new(3000);
    env.add_task("t1", TaskState::Ready);
    let warn = |rule: &str, line: Option<u32>, msg: &str, fix: &str| StaticFinding {
        rule: rule.into(),
        severity: Severity::Warn,
        file: "src/lib.rs".into(),
        line,
        message: msg.into(),
        fix: fix.into(),
    };
    let gate = Arc::new(StaticGate {
        cfg_hash: aaf_engine::gate::config_hash(&repo),
        findings: vec![
            warn(
                "RUST-001",
                Some(42),
                "avoid unwrap",
                "propagate error with ?",
            ),
            warn("RUST-002", Some(88), "avoid clone", "borrow slice"),
            warn("RUST-003", None, "unused helper", ""),
        ],
        unresolved: vec!["dynamic camera permission".into()],
    });
    let deps = env.deps(
        ScriptedRunner::from_texts(&["impl 1", "impl 2"]),
        gate,
        vec![],
        repo,
        1,
    );
    let ar = ScriptedRunner::from_texts(&[&pass_json()]);
    let audit = mk_audit("gemini", ar.clone());

    // Pass 1: mech = 100 - 6 (3 warns) - 3 (1 unresolved) = 91 < 95 -> audit_rework, 0 LLM calls.
    let s1 = run_with_audit(&deps, Some(&audit), Mode::Once, &hooks_stop()).unwrap();
    assert!(s1.requeued == vec!["t1"] && s1.review.is_empty());
    assert_eq!(ar.1.lock().unwrap().len(), 0);
    assert_eq!(env.git.integrates.load(Ordering::SeqCst), 0);

    let events = env.db.events_for_task("t1", 50).unwrap();
    let ev = events
        .iter()
        .find(|e| e.kind == EventKind::CrossAudit)
        .unwrap();
    let p: serde_json::Value = serde_json::from_str(&ev.payload_json).unwrap();
    assert_eq!(p["short_circuited"], serde_json::json!(true));
    assert_eq!(p["mechanical_score"], serde_json::json!(91));
    assert_eq!(p["effective_score"], serde_json::json!(91));
    assert!(p["llm_average"].is_null());
    assert!(
        p["verdict"]
            .as_str()
            .is_some_and(|v| v.starts_with("fail:"))
    );

    let t1 = env.db.get_task("t1").unwrap();
    assert_eq!(t1.state, TaskState::Ready);
    assert!(t1.context_notes.iter().any(|n| {
        n.starts_with("audit_rework:")
            && n.contains("RUST-001")
            && n.contains("src/lib.rs:42")
            && n.contains("propagate error with ?")
            && n.contains("dynamic camera permission")
    }));
    assert_eq!(env.db.get_budget("t1").unwrap().cycles_used, 1);

    // Pass 2: max_audit_reworks=1 exhausted -> falls through to Next::Unblock, still 0 LLM calls.
    let _s2 = run_with_audit(&deps, Some(&audit), Mode::Once, &hooks_stop()).unwrap();
    assert_eq!(ar.1.lock().unwrap().len(), 0);
    assert_eq!(env.git.integrates.load(Ordering::SeqCst), 0);
    let t2 = env.db.get_task("t1").unwrap();
    assert!(
        t2.context_notes
            .iter()
            .any(|n| n.contains("post-gate cross-audit rejected the changes"))
    );
}
