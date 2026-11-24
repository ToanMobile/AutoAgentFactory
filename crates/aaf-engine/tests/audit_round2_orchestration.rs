#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Round-2 auto-orchestration regression tests: router cooldown clamping,
//! LeastUsed audit account independence, human unblock reset markers,
//! scheduler non-HOL-blocking across work/audit pools, auditor network retry,
//! and merge-time unverified gate handling.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::blocked::UnblockTier;
use aaf_domain::config::{AccountSelection, RepoConfig, UnblockerConfig};
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::gate::{
    GateMode, GateStatus, HeadLockReport, LockStatus, RECEIPT_SCHEMA_VERSION, Receipt,
    StoreComplianceReport,
};
use aaf_domain::ports::{
    AccountStore, AgentOutcome, AgentRequest, AgentRunner, Clock, EventSink, InfraFailure,
    InfraKind, LimitHit, PortError, PortResult, RunStore, TaskStore,
};
use aaf_domain::run::{RunRecord, RunStatus};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::accounts::Router;
use aaf_engine::cross_audit::build_checklist;
use aaf_engine::db::Db;
use aaf_engine::factory::adapters::RouterPool;
use aaf_engine::factory::{
    AccountPool, AuditBackend, Deps, GateRunner, GitOps, Grant, Hooks, Integration, Mode, Notifier,
    Purpose, SelectingPool, Settings, is_frozen, run, run_with_audit,
};
use aaf_engine::git::{MergeOutcome, PrOutcome, TaskWorktree};
use aaf_engine::unblocker::{FailureSignal, Ladder};
use aaf_engine::webhook::WebhookEvent;
use tempfile::TempDir;

struct AtomicClock(AtomicI64);
impl AtomicClock {
    fn new(now: i64) -> Self {
        Self(AtomicI64::new(now))
    }
    fn set(&self, now: i64) {
        self.0.store(now, Ordering::SeqCst);
    }
    fn advance(&self, delta: i64) {
        self.0.fetch_add(delta, Ordering::SeqCst);
    }
}
impl Clock for AtomicClock {
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

struct StaticPool {
    profiles: Vec<AccountProfile>,
    exhausted: bool,
}
impl AccountPool for StaticPool {
    fn acquire(&self) -> PortResult<Option<Grant>> {
        Ok((!self.exhausted).then(|| Grant::new(self.profiles.first().cloned(), None)))
    }
    fn report_limit(&self, _: &str, _: &LimitHit) -> PortResult<()> {
        Ok(())
    }
    fn profiles(&self) -> PortResult<Vec<AccountProfile>> {
        Ok(self.profiles.clone())
    }
}

struct ScriptedRunner(Mutex<VecDeque<AgentOutcome>>);
impl AgentRunner for ScriptedRunner {
    fn run(&self, _: &AgentRequest) -> PortResult<AgentOutcome> {
        let next = self.0.lock().unwrap().pop_front();
        Ok(next.unwrap_or_else(|| mk_outcome(0, "ok", "", None)))
    }
}

fn runner(outcomes: Vec<AgentOutcome>) -> Arc<ScriptedRunner> {
    Arc::new(ScriptedRunner(Mutex::new(outcomes.into())))
}

fn mk_outcome(code: i32, text: &str, tail: &str, infra: Option<InfraFailure>) -> AgentOutcome {
    AgentOutcome {
        exit_code: Some(code),
        timed_out: false,
        final_text: text.into(),
        tokens_in: 10,
        tokens_out: 5,
        limit_hit: None,
        infra,
        log_tail: tail.into(),
    }
}

fn ok(text: &str) -> AgentOutcome {
    mk_outcome(0, text, "", None)
}

fn sample_spec() -> ImmutableSpec {
    ImmutableSpec {
        description: "Verify orchestration invariants".into(),
        acceptance: vec!["all checks pass".into()],
    }
}

fn audit_json(pass: bool, evidence: &str) -> String {
    let mut answers: Vec<serde_json::Value> = build_checklist(&sample_spec(), false)
        .iter()
        .map(|item| serde_json::json!({ "id": item.id, "answer": "no" }))
        .collect();
    if !pass && let Some(first) = answers.first_mut() {
        first["answer"] = serde_json::json!("yes");
        first["evidence"] = serde_json::json!(evidence);
    }
    serde_json::json!({ "answers": answers }).to_string()
}

struct FakeGit {
    base: PathBuf,
    counter: AtomicUsize,
    base_calls: AtomicUsize,
    regate: bool,
}
impl GitOps for FakeGit {
    fn task_worktree(&self, id: &str) -> PortResult<TaskWorktree> {
        Ok(TaskWorktree {
            path: self.base.join(format!("wt-{id}")),
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
        Ok("diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,1 +1,2 @@\n fn base() {}\n+line\n".into())
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
        if self.regate {
            Ok(format!(
                "base-{}",
                self.base_calls.fetch_add(1, Ordering::SeqCst)
            ))
        } else {
            Ok("base0".into())
        }
    }
    fn merge(
        &self,
        wt: &TaskWorktree,
        regate: &dyn Fn(&Path) -> Result<bool, String>,
    ) -> PortResult<MergeOutcome> {
        if self.regate && !regate(&wt.path).map_err(PortError::Io)? {
            return Ok(MergeOutcome::GateFailed);
        }
        Ok(MergeOutcome::Merged {
            new_base_sha: "base-merged".into(),
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
            reason: "test".into(),
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

struct FlexibleGate {
    data_dir: PathBuf,
    locks: HashMap<String, String>,
    statuses: Mutex<VecDeque<(GateStatus, Vec<String>)>>,
}
impl GateRunner for FlexibleGate {
    fn run(&self, tree: &Path) -> PortResult<Receipt> {
        let name = tree.file_name().unwrap().to_string_lossy().to_string();
        let (status, reasons) = if let Some(next) = self.statuses.lock().unwrap().pop_front() {
            next
        } else if self
            .locks
            .get(&name)
            .is_some_and(|fp| !aaf_fp::approval_path(&self.data_dir, tree, fp).is_file())
        {
            (GateStatus::Blocked, vec!["head lock".into()])
        } else {
            (GateStatus::Clear, Vec::new())
        };
        Ok(Receipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            repo_root: tree.to_string_lossy().into_owned(),
            tree_fp: format!("git-tree:{name}"),
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

fn mk_profile(tmp: &TempDir, id: &str, provider: ProviderKind) -> AccountProfile {
    let dir = tmp.path().join(format!("acct-{id}"));
    std::fs::create_dir_all(&dir).unwrap();
    AccountProfile {
        id: id.into(),
        provider,
        config_dir: dir.to_string_lossy().into_owned(),
        name: id.into(),
    }
}

fn hooks_once() -> Hooks<'static> {
    Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    }
}

fn mk_audit(
    outcomes: Vec<AgentOutcome>,
    profiles: Vec<AccountProfile>,
    exhausted: bool,
) -> AuditBackend {
    AuditBackend {
        provider: "codex".into(),
        runner: runner(outcomes),
        accounts: Arc::new(StaticPool {
            profiles,
            exhausted,
        }),
        hook_required: false,
    }
}

struct Env {
    tmp: TempDir,
    db: Db,
    clock: Arc<AtomicClock>,
}

impl Env {
    fn new(now: i64) -> Self {
        let tmp = TempDir::new().unwrap();
        let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
        Self {
            tmp,
            db,
            clock: Arc::new(AtomicClock::new(now)),
        }
    }

    fn task_in(&self, id: &str, repo: &str, state: TaskState) {
        let mut task = Task::new_root(id, repo, id, sample_spec(), Priority::P1, 1);
        task.state = state;
        self.db.insert_task(&task).unwrap();
    }

    fn task(&self, id: &str, state: TaskState) {
        self.task_in(id, "/fake/repo", state);
    }

    fn deps(
        &self,
        work_runner: Arc<dyn AgentRunner>,
        locks: HashMap<String, String>,
        statuses: Vec<(GateStatus, Vec<String>)>,
        auto_merge: bool,
    ) -> Deps {
        let acct = mk_profile(&self.tmp, "claude-a", ProviderKind::ClaudeCode);
        Deps {
            store: Arc::new(self.db.clone()),
            clock: self.clock.clone(),
            runner: work_runner,
            accounts: Arc::new(StaticPool {
                profiles: vec![acct],
                exhausted: false,
            }),
            git: Arc::new(FakeGit {
                base: self.tmp.path().to_path_buf(),
                counter: AtomicUsize::new(0),
                base_calls: AtomicUsize::new(0),
                regate: auto_merge,
            }),
            gate: Arc::new(FlexibleGate {
                data_dir: self.tmp.path().to_path_buf(),
                locks,
                statuses: Mutex::new(statuses.into()),
            }),
            notifier: Arc::new(NullNotifier),
            settings: Settings {
                repo_root: "/fake/repo".into(),
                repo: RepoConfig::default(),
                max_parallel: 1,
                auto_merge,
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

#[test]
fn router_report_limit_clamps_stale_or_equal_reset_at_to_60s() {
    let env = Env::new(10_000);
    let router = Router::new(
        &env.db,
        &env.db,
        &*env.clock,
        env.tmp.path().join("leases"),
        1,
    );
    let profs = [
        mk_profile(&env.tmp, "acct-a", ProviderKind::ClaudeCode),
        mk_profile(&env.tmp, "acct-b", ProviderKind::ClaudeCode),
    ];
    router.sync(&profs).unwrap();
    for (acct, reset) in [("acct-a", 10_000), ("acct-b", 5_000)] {
        let hit = LimitHit {
            reset_at: Some(reset),
            raw: "429".into(),
        };
        router.report_limit(acct, &hit).unwrap();
    }
    let all = env.db.list_accounts().unwrap();
    assert!(all.iter().all(|s| s.cooldown_until == Some(10_060)));
    env.clock.set(10_059);
    assert!(router.next().unwrap().is_none());
    env.clock.set(10_060);
    assert_eq!(router.next().unwrap().unwrap().0.id, "acct-a");
}

#[test]
fn least_used_audit_prefers_non_author_account_and_falls_back() {
    let env = Env::new(10_000);
    for id in ["acct-a", "acct-b"] {
        env.db
            .upsert_account(&mk_profile(&env.tmp, id, ProviderKind::ClaudeCode))
            .unwrap();
    }
    let repo = env.tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let repo_s = repo.to_string_lossy().into_owned();
    env.task_in("t0", &repo_s, TaskState::Ready);
    env.task_in("t1", &repo_s, TaskState::Ready);
    for (rid, tid, acct, tokens, ts) in [
        ("r0", "t0", "acct-b", 5_000, 9_900),
        ("r1", "t1", "acct-a", 0, 9_950),
    ] {
        env.db
            .insert_run(&RunRecord {
                id: rid.into(),
                task_id: tid.into(),
                account_id: Some(acct.into()),
                executor: "claude".into(),
                worktree_path: String::new(),
                branch: String::new(),
                status: RunStatus::Succeeded,
                exit_code: Some(0),
                tokens_in: tokens,
                tokens_out: 0,
                started_at: ts,
                ended_at: Some(ts + 5),
            })
            .unwrap();
    }
    let store = Arc::new(env.db.clone());
    let rp = RouterPool {
        accounts: store.clone(),
        runs: store.clone(),
        clock: env.clock.clone(),
        leases_dir: env.tmp.path().join("leases"),
        max_concurrent_per_account: 1,
        provider: Some(ProviderKind::ClaudeCode),
    };
    let pool = SelectingPool::new(rp, AccountSelection::LeastUsed, &repo, store);
    let pick = |p| {
        pool.acquire_for("t1", p)
            .unwrap()
            .unwrap()
            .account_id()
            .map(str::to_string)
    };
    assert_eq!(pick(Purpose::Work).as_deref(), Some("acct-a"));
    assert_eq!(pick(Purpose::Audit).as_deref(), Some("acct-b"));
    env.db.set_cooldown("acct-b", Some(20_000)).unwrap();
    assert_eq!(pick(Purpose::Audit).as_deref(), Some("acct-a"));
}

#[test]
fn human_unblock_events_and_notes_reset_audit_reworks_and_ladder() {
    for why in [
        "aaf ui unblock",
        "aaf telegram unblock",
        "aaf cockpit unblock",
    ] {
        let env = Env::new(2_000);
        env.task("t1", TaskState::Ready);
        let fail = audit_json(false, "src/lib.rs:2: fix edge case");
        let locks = HashMap::from([("wt-t1".into(), "fp1".into())]);
        let mut deps = env.deps(runner(vec![ok("i1"), ok("i2")]), locks, vec![], false);
        deps.settings.repo.pipeline.max_audit_reworks = 1;
        let audit = mk_audit(vec![ok(&fail), ok(&fail)], vec![], false);
        let s1 = run_with_audit(&deps, Some(&audit), Mode::Once, &hooks_once()).unwrap();
        assert_eq!(s1.requeued, vec!["t1"]);
        let p = serde_json::json!({ "to": "ready", "why": why }).to_string();
        env.db
            .emit(&FactoryEvent::new(EventKind::TaskStateChanged, p, 2_010).for_task("t1"))
            .unwrap();
        let s2 = run_with_audit(&deps, Some(&audit), Mode::Once, &hooks_once()).unwrap();
        assert_eq!(s2.requeued, vec!["t1"]);
        let t1 = env.db.get_task("t1").unwrap();
        assert_eq!(
            t1.context_notes
                .iter()
                .filter(|n| n.starts_with("audit_rework:"))
                .count(),
            2,
            "{why}"
        );
    }
    let env = Env::new(1_000);
    env.task("t1", TaskState::Ready);
    let cfg = UnblockerConfig::default();
    let ladder = Ladder::new(&env.db, &env.db, &env.db, &env.db, &*env.clock, &cfg);
    let mut signal = FailureSignal::empty();
    signal.outcome = Some(mk_outcome(1, "", "compile error", None));
    assert_eq!(
        ladder.next_action("t1", &signal, None).unwrap().tier,
        UnblockTier::T3FreshContext
    );
    env.db
        .append_context_note("t1", "unblocked: fixed toolchain", 1_005)
        .unwrap();
    assert_eq!(
        ladder.next_action("t1", &signal, None).unwrap().tier,
        UnblockTier::T3FreshContext
    );
}

#[test]
fn scheduler_does_not_hol_block_work_when_audit_pool_is_exhausted() {
    let env = Env::new(3_000);
    env.task("t-audit", TaskState::Gating);
    env.task("t-work", TaskState::Ready);
    let locks = HashMap::from([("wt-t-audit".into(), "fp-audit".into())]);
    let mut deps = env.deps(runner(vec![ok("work")]), locks, vec![], false);
    deps.settings.max_parallel = 2;
    let audit = mk_audit(
        vec![],
        vec![mk_profile(&env.tmp, "codex-busy", ProviderKind::Codex)],
        true,
    );
    let summary = run_with_audit(&deps, Some(&audit), Mode::Once, &hooks_once()).unwrap();
    assert_eq!(summary.review, vec!["t-work"]);
    assert_eq!(summary.agent_runs, 1);
    assert!(summary.waiting_for_account >= 1, "{summary:?}");
    assert_eq!(env.db.get_task("t-work").unwrap().state, TaskState::Review);
    assert_eq!(env.db.get_task("t-audit").unwrap().state, TaskState::Gating);
}

#[test]
fn auditor_network_infra_retries_in_auto_mode_and_unverified_merge_blocks_untried() {
    let env = Env::new(4_000);
    env.task("t1", TaskState::Gating);
    let locks = HashMap::from([("wt-t1".into(), "fp-net".into())]);
    let deps = env.deps(runner(vec![]), locks, vec![], false);
    let net_fail = mk_outcome(
        1,
        "",
        "HTTP 503",
        Some(InfraFailure {
            kind: InfraKind::Network,
            raw: "HTTP 503".into(),
        }),
    );
    let audit = mk_audit(vec![net_fail, ok(&audit_json(true, ""))], vec![], false);
    let ticks = AtomicUsize::new(0);
    let (db_c, clock_c) = (env.db.clone(), env.clock.clone());
    let idle = || {
        if db_c.get_task("t1").unwrap().state == TaskState::Review {
            return false;
        }
        clock_c.advance(120);
        ticks.fetch_add(1, Ordering::SeqCst) < 3
    };
    let hooks = Hooks {
        idle: &idle,
        reap: &|_| Ok(()),
    };
    let summary = run_with_audit(&deps, Some(&audit), Mode::Auto, &hooks).unwrap();
    assert!(summary.halted.is_none(), "{summary:?}");
    assert_eq!(summary.review, vec!["t1"]);

    let env2 = Env::new(5_000);
    env2.task("t2", TaskState::Ready);
    let statuses = vec![
        (GateStatus::Clear, Vec::new()),
        (GateStatus::Unverified, vec!["no test command".into()]),
    ];
    let mut deps2 = env2.deps(runner(vec![ok("done")]), HashMap::new(), statuses, true);
    deps2
        .settings
        .repo
        .unblocker
        .global_freeze_consecutive_fails = 1;
    let s2 = run(&deps2, Mode::Once, &hooks_once()).unwrap();
    assert_eq!(s2.blocked, vec!["t2"]);
    assert!(!is_frozen(&env2.db).unwrap());
    let t2 = env2.db.get_task("t2").unwrap();
    assert_eq!((t2.state, t2.unblock_cycles), (TaskState::Blocked, 0));
    let events = env2.db.events_for_task("t2", 50).unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == EventKind::GateResult && e.payload_json.contains("UNVERIFIED"))
    );
}
