#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::budget::RootBudget;
use aaf_domain::config::RepoConfig;
use aaf_domain::gate::{
    GateMode, GateStatus, HeadLockReport, LockStatus, Receipt, StoreComplianceReport,
};
use aaf_domain::ports::{
    AgentOutcome, AgentRequest, AgentRunner, BudgetStore, Clock, InfraFailure, InfraKind, KvStore,
    LimitHit, PortResult, RunStore, TaskStore,
};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::executors::claude::StreamParser;
use aaf_engine::executors::generic::classify_generic_failure;
use aaf_engine::factory::{
    AccountPool, Deps, GateRunner, GitOps, Grant, Hooks, Integration, Mode, Notifier, Settings, run,
};
use aaf_engine::git::{MergeOutcome, PrOutcome, TaskWorktree};
use aaf_engine::unblocker::QuotaRoute;

struct StepClock(AtomicI64);
impl Clock for StepClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct StubPool;
impl AccountPool for StubPool {
    fn acquire(&self) -> PortResult<Option<Grant>> {
        Ok(Some(Grant::new(
            Some(AccountProfile {
                id: "acct-1".into(),
                provider: ProviderKind::ClaudeCode,
                config_dir: "/tmp/acct-1".into(),
                name: "acct-1".into(),
            }),
            None,
        )))
    }
    fn report_limit(&self, _id: &str, _hit: &LimitHit) -> PortResult<()> {
        Ok(())
    }
    fn after_limit(&self) -> PortResult<Option<QuotaRoute>> {
        Ok(Some(QuotaRoute::Wait))
    }
    fn provider_account_count(&self) -> PortResult<usize> {
        Ok(1)
    }
    fn profiles(&self) -> PortResult<Vec<AccountProfile>> {
        Ok(Vec::new())
    }
}

struct StubGit;
impl GitOps for StubGit {
    fn task_worktree(&self, id: &str) -> PortResult<TaskWorktree> {
        Ok(TaskWorktree {
            path: std::path::PathBuf::from(format!("/tmp/wt/{id}")),
            branch: format!("aaf/{id}"),
        })
    }
    fn cleanup(&self, _id: &str) -> PortResult<()> {
        Ok(())
    }
    fn commit_all(&self, _wt: &TaskWorktree, _msg: &str) -> PortResult<()> {
        Ok(())
    }
    fn has_changes(&self, _wt: &TaskWorktree) -> PortResult<bool> {
        Ok(false)
    }
    fn diff(&self, _wt: &TaskWorktree) -> PortResult<String> {
        Ok(String::new())
    }
    fn integrate(&self, _ids: &[String]) -> PortResult<Integration> {
        unreachable!()
    }
    fn discard(&self, _i: &Integration) -> PortResult<()> {
        Ok(())
    }
    fn rebase(&self, _wt: &TaskWorktree) -> PortResult<Vec<String>> {
        Ok(Vec::new())
    }
    fn base_sha(&self) -> PortResult<String> {
        Ok("a".repeat(40))
    }
    fn merge(
        &self,
        _wt: &TaskWorktree,
        _r: &dyn Fn(&std::path::Path) -> Result<bool, String>,
    ) -> PortResult<MergeOutcome> {
        unreachable!()
    }
    fn open_pr(
        &self,
        _wt: &TaskWorktree,
        _t: &str,
        _r: &Receipt,
        _n: Option<&str>,
    ) -> PortResult<PrOutcome> {
        unreachable!()
    }
    fn fetch_base(&self) -> PortResult<()> {
        Ok(())
    }
    fn is_merged(&self, _id: &str) -> PortResult<bool> {
        Ok(false)
    }
}

struct StubGate;
impl GateRunner for StubGate {
    fn run(&self, _tree: &std::path::Path) -> PortResult<Receipt> {
        Ok(Receipt {
            schema_version: aaf_domain::gate::RECEIPT_SCHEMA_VERSION,
            repo_root: "/tmp/r".into(),
            head_sha: Some("a".repeat(40)),
            tree_fp: "fp".into(),
            base_sha: None,
            mode: GateMode::Scoped,
            status: GateStatus::Clear,
            reasons: Vec::new(),
            commands: Vec::new(),
            head_lock: HeadLockReport {
                status: LockStatus::Clear,
                lock_fp: String::new(),
                locked_changes: Vec::new(),
                approval: None,
            },
            static_findings: Vec::new(),
            suppressions: 0,
            store_compliance: StoreComplianceReport::not_applicable(),
            config_hash: "h".into(),
            created_at: 1,
        })
    }
    fn head_lock(&self, _tree: &std::path::Path) -> PortResult<HeadLockReport> {
        Ok(HeadLockReport {
            status: LockStatus::Clear,
            lock_fp: String::new(),
            locked_changes: Vec::new(),
            approval: None,
        })
    }
}

struct StubNotifier;
impl Notifier for StubNotifier {
    fn notify(&self, _e: &aaf_engine::webhook::WebhookEvent) -> Result<bool, String> {
        Ok(false)
    }
}

struct AdvancingRunner {
    clock: Arc<StepClock>,
    advance_secs: i64,
    outcomes: Mutex<Vec<AgentOutcome>>,
    calls: AtomicUsize,
}

impl AgentRunner for AdvancingRunner {
    fn run(&self, _req: &AgentRequest) -> PortResult<AgentOutcome> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.clock.0.fetch_add(self.advance_secs, Ordering::SeqCst);
        let mut q = self.outcomes.lock().unwrap();
        Ok(if q.len() > 1 {
            q.remove(0)
        } else {
            q[0].clone()
        })
    }
    fn reports_tokens(&self) -> bool {
        true
    }
}

fn make_deps(
    tmp: &std::path::Path,
    db: Arc<Db>,
    clock: Arc<StepClock>,
    runner: Arc<dyn AgentRunner>,
) -> Deps {
    let mut repo_cfg = RepoConfig::default();
    repo_cfg.unblocker.max_wall_clock_secs = 500;
    repo_cfg.unblocker.max_token_budget = 100_000;
    Deps {
        store: db,
        git: Arc::new(StubGit),
        runner,
        gate: Arc::new(StubGate),
        accounts: Arc::new(StubPool),
        notifier: Arc::new(StubNotifier),
        clock,
        settings: Settings {
            repo_root: tmp.to_string_lossy().into_owned(),
            data_dir: tmp.join("data"),
            home: tmp.join("home"),
            agent_path: "aaf-hook".into(),
            max_parallel: 1,
            auto_merge: false,
            mobile: false,
            sandboxed: false,
            hook_required: false,
            repo: repo_cfg,
            owner_github_allowlist: Vec::new(),
        },
    }
}

#[test]
fn zero_token_network_outage_does_not_burn_wall_clock_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(&tmp.path().join("aaf.db")).unwrap());
    let spec = ImmutableSpec {
        description: "desc".into(),
        acceptance: vec!["ok".into()],
    };
    let mut task = Task::new_root(
        "t-net",
        tmp.path().to_string_lossy(),
        "net task",
        spec,
        Priority::P1,
        1_000,
    );
    task.state = TaskState::Ready;
    db.insert_task(&task).unwrap();

    let clock = Arc::new(StepClock(AtomicI64::new(1_000)));
    let enotfound = AgentOutcome {
        exit_code: Some(1),
        timed_out: false,
        final_text: String::new(),
        tokens_in: 0,
        tokens_out: 0,
        limit_hit: None,
        infra: Some(InfraFailure {
            kind: InfraKind::Network,
            raw: "API Error: Can't reach the API server — check your internet or DNS (ENOTFOUND)"
                .into(),
        }),
        log_tail: "ENOTFOUND".into(),
    };
    let runner = Arc::new(AdvancingRunner {
        clock: Arc::clone(&clock),
        advance_secs: 200,
        outcomes: Mutex::new(vec![enotfound]),
        calls: AtomicUsize::new(0),
    });
    let deps = make_deps(tmp.path(), Arc::clone(&db), Arc::clone(&clock), runner);
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };

    // Simulate 5 sequential supervisor restarts during a prolonged DNS outage (5 * 200s = 1000s > 500s limit).
    for _ in 0..5 {
        let _ = run(&deps, Mode::Once, &hooks).unwrap();
    }

    let budget = db.get_budget("t-net").unwrap();
    assert_eq!(
        budget.wall_secs_used, 0,
        "0-token DNS errors must not burn wall budget"
    );
    assert!(!budget.frozen, "task must not freeze during network outage");
    assert_eq!(db.get_task("t-net").unwrap().state, TaskState::Ready);
    let runs = db.runs_for_task("t-net").unwrap();
    assert_eq!(runs.len(), 5);
    assert!(
        runs.iter()
            .all(|r| r.ended_at.unwrap() - r.started_at == 200),
        "runs telemetry must still record true wall duration"
    );
}

#[test]
fn mid_run_limit_hit_with_tokens_still_charges_wall_and_tokens() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(&tmp.path().join("aaf.db")).unwrap());
    let spec = ImmutableSpec {
        description: "desc".into(),
        acceptance: vec!["ok".into()],
    };
    let mut task = Task::new_root(
        "t-mid",
        tmp.path().to_string_lossy(),
        "mid limit",
        spec,
        Priority::P1,
        1_000,
    );
    task.state = TaskState::Ready;
    db.insert_task(&task).unwrap();

    let clock = Arc::new(StepClock(AtomicI64::new(1_000)));
    let outcome = AgentOutcome {
        exit_code: Some(1),
        timed_out: false,
        final_text: "partial".into(),
        tokens_in: 4_000,
        tokens_out: 1_000,
        limit_hit: Some(LimitHit {
            reset_at: Some(2_000),
            raw: "429".into(),
        }),
        infra: None,
        log_tail: "429".into(),
    };
    let runner = Arc::new(AdvancingRunner {
        clock: Arc::clone(&clock),
        advance_secs: 250,
        outcomes: Mutex::new(vec![outcome]),
        calls: AtomicUsize::new(0),
    });
    let deps = make_deps(tmp.path(), Arc::clone(&db), Arc::clone(&clock), runner);
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };
    let _ = run(&deps, Mode::Once, &hooks).unwrap();

    let budget = db.get_budget("t-mid").unwrap();
    assert_eq!(budget.tokens_used, 5_000);
    assert_eq!(budget.wall_secs_used, 250);
}

#[test]
fn subtasks_of_one_exhausted_root_count_only_once_toward_factory_freeze() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(&tmp.path().join("aaf.db")).unwrap());
    let spec = ImmutableSpec {
        description: "desc".into(),
        acceptance: vec!["ok".into()],
    };
    let mut root = Task::new_root(
        "t-root",
        tmp.path().to_string_lossy(),
        "root",
        spec.clone(),
        Priority::P1,
        1_000,
    );
    root.state = TaskState::Ready;
    db.insert_task(&root).unwrap();
    for i in 1..=3 {
        let mut sub = Task::new_subtask(
            format!("t-root-s{i}"),
            &root,
            format!("s{i}"),
            spec.clone(),
            1_000,
        )
        .unwrap();
        sub.state = TaskState::Ready;
        db.insert_task(&sub).unwrap();
        db.add_dep(&root.id, &sub.id).unwrap();
    }
    // Pre-exhaust the root budget (not yet frozen).
    let mut b = RootBudget::new("t-root", &RepoConfig::default().unblocker);
    b.wall_secs_limit = 100;
    b.wall_secs_used = 200;
    db.put_budget(&b).unwrap();

    let clock = Arc::new(StepClock(AtomicI64::new(1_000)));
    let ok_out = AgentOutcome {
        exit_code: Some(0),
        timed_out: false,
        final_text: "done".into(),
        tokens_in: 0,
        tokens_out: 0,
        limit_hit: None,
        infra: None,
        log_tail: String::new(),
    };
    let runner = Arc::new(AdvancingRunner {
        clock: Arc::clone(&clock),
        advance_secs: 10,
        outcomes: Mutex::new(vec![ok_out]),
        calls: AtomicUsize::new(0),
    });
    let deps = make_deps(tmp.path(), Arc::clone(&db), Arc::clone(&clock), runner);
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };
    let _ = run(&deps, Mode::Once, &hooks).unwrap();

    let fails: u32 = db
        .get_meta("consecutive_root_fails")
        .unwrap()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    assert_eq!(
        fails, 1,
        "one exhausted root with 3 subtasks must count once, not 3x"
    );
    assert_ne!(
        db.get_meta("factory_frozen").unwrap().as_deref(),
        Some("1"),
        "factory must not freeze from a single root's subtasks"
    );
}

#[test]
fn claude_and_generic_classify_enotfound_as_network_infra() {
    let mut parser = StreamParser::default();
    parser.feed_line(
        r#"{"type":"result","is_error":true,"terminal_reason":"api_error","result":"API Error: Can't reach the API server — check your internet or DNS (ENOTFOUND)"}"#,
    );
    let summary = parser.summary();
    assert_eq!(
        summary.infra.as_ref().map(|i| i.kind),
        Some(InfraKind::Network)
    );

    let (limit, infra) = classify_generic_failure(
        Some(1),
        false,
        "Error: getaddrinfo ENOTFOUND api.anthropic.com",
    );
    assert!(limit.is_none());
    assert_eq!(infra.as_ref().map(|i| i.kind), Some(InfraKind::Network));

    let (_, timed_out_infra) = classify_generic_failure(Some(143), true, "ENOTFOUND");
    assert!(
        timed_out_infra.is_none(),
        "timed_out must never classify as infra"
    );
}

#[test]
fn sync_repo_budget_limits_updates_active_unfrozen_without_touching_usage_or_frozen() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    let repo_a = "/repo/a";
    let repo_b = "/repo/b";
    let mk = |id: &str, repo: &str, state: TaskState| {
        let mut t = Task::new_root(
            id,
            repo,
            id,
            ImmutableSpec {
                description: "d".into(),
                acceptance: vec!["ok".into()],
            },
            Priority::P1,
            1_000,
        );
        t.state = state;
        t
    };

    // Active unfrozen root in repo_a with stale 3600s / 500k limits and existing usage.
    db.insert_task(&mk("t-active", repo_a, TaskState::Gating))
        .unwrap();
    let mut b_active = RootBudget::new("t-active", &aaf_domain::config::UnblockerConfig::default());
    b_active.token_limit = 500_000;
    b_active.wall_secs_limit = 3_600;
    b_active.cycles_limit = 5;
    b_active.tokens_used = 123_456;
    b_active.wall_secs_used = 2_773;
    b_active.cycles_used = 1;
    db.put_budget(&b_active).unwrap();

    // Frozen blocked root in repo_a must NOT be touched or unfrozen.
    db.insert_task(&mk("t-frozen", repo_a, TaskState::Blocked))
        .unwrap();
    let mut b_frozen = RootBudget::new("t-frozen", &aaf_domain::config::UnblockerConfig::default());
    b_frozen.token_limit = 500_000;
    b_frozen.wall_secs_limit = 3_600;
    b_frozen.frozen = true;
    db.put_budget(&b_frozen).unwrap();

    // Active unfrozen root in repo_b must NOT be touched when syncing repo_a.
    db.insert_task(&mk("t-other", repo_b, TaskState::Ready))
        .unwrap();
    let mut b_other = RootBudget::new("t-other", &aaf_domain::config::UnblockerConfig::default());
    b_other.token_limit = 500_000;
    b_other.wall_secs_limit = 3_600;
    db.put_budget(&b_other).unwrap();

    let cfg = aaf_domain::config::UnblockerConfig {
        max_token_budget: 1_000_000,
        max_wall_clock_secs: 10_800,
        max_unblock_cycles: 8,
        ..Default::default()
    };

    let updated = db.sync_repo_budget_limits(repo_a, &cfg).unwrap();
    assert_eq!(updated, 1);

    let got_active = db.get_budget("t-active").unwrap();
    assert_eq!(got_active.token_limit, 1_000_000);
    assert_eq!(got_active.wall_secs_limit, 10_800);
    assert_eq!(got_active.cycles_limit, 8);
    assert_eq!(got_active.tokens_used, 123_456, "usage must be preserved");
    assert_eq!(got_active.wall_secs_used, 2_773, "usage must be preserved");
    assert_eq!(got_active.cycles_used, 1, "usage must be preserved");
    assert!(!got_active.frozen);

    let got_frozen = db.get_budget("t-frozen").unwrap();
    assert_eq!(got_frozen.wall_secs_limit, 3_600);
    assert!(got_frozen.frozen, "sync must never auto-unfreeze");

    let got_other = db.get_budget("t-other").unwrap();
    assert_eq!(got_other.wall_secs_limit, 3_600);
}

#[test]
fn ensure_budget_if_missing_preserves_existing_usage() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    let cfg = aaf_domain::config::UnblockerConfig::default();
    let first = db
        .ensure_budget_if_missing(&RootBudget::new("t-race", &cfg))
        .unwrap();
    assert_eq!(first.tokens_used, 0);
    db.add_usage("t-race", 42_000, 300, 1).unwrap();

    // A racing subtask calling ensure_budget_if_missing must NOT reset usage to 0.
    let second = db
        .ensure_budget_if_missing(&RootBudget::new("t-race", &cfg))
        .unwrap();
    assert_eq!(second.tokens_used, 42_000);
    assert_eq!(second.wall_secs_used, 300);
    assert_eq!(second.cycles_used, 1);
}
