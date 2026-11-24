#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Wave 2 Scheduler, Plan Debate, Rate-Limit Isolation, and Antigravity
//! exclusive lock integration tests (`docs/PLAN_DUAL_FRONTIER.md` §F Wave 2).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::blocked::BlockedReason;
use aaf_domain::config::RepoConfig;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::gate::{
    GateMode, GateStatus, HeadLockReport, LockStatus, RECEIPT_SCHEMA_VERSION, Receipt,
    StoreComplianceReport,
};
use aaf_domain::pipeline::PlanContract;
use aaf_domain::ports::{
    AccountStore, AgentOutcome, AgentRequest, AgentRunner, Clock, EventSink, KvStore, LimitHit,
    META_CONSECUTIVE_ROOT_FAILS, PortResult, RunStore, TaskStore,
};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::accounts::RunHistory;
use aaf_engine::db::Db;
use aaf_engine::factory::adapters::WebhookNotifier;
use aaf_engine::factory::prompt::PLAN_EXECUTOR;
use aaf_engine::factory::{
    AccountPool, AntigravityLockedPool, Deps, GateRunner, GitOps, Grant, Hooks, Integration, Mode,
    Purpose, RunSummary, Settings, StageRouter, StageTarget, run_with_router,
};
use aaf_engine::git::{MergeOutcome, PrOutcome, TaskWorktree};
use aaf_engine::plan_debate::prompt::{PLAN_BEGIN, PLAN_END};
use aaf_engine::plan_debate::{
    MAX_CONSECUTIVE_TURN_FAILS, PLAN_DEBATE_STALLED_PREFIX, canonical_plan_hash,
};
use aaf_engine::unblocker::QuotaRoute;
use serde_json::{Value, json};
use tempfile::TempDir;

type RegateFn<'a> = &'a dyn Fn(&Path) -> Result<bool, String>;

struct StubPool(Vec<AccountProfile>);
impl AccountPool for StubPool {
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

#[derive(Default)]
struct QueueRunner(Mutex<(VecDeque<AgentOutcome>, Vec<AgentRequest>)>);
impl QueueRunner {
    fn push(&self, text: impl AsRef<str>) {
        self.0
            .lock()
            .unwrap()
            .0
            .push_back(out(0, text.as_ref(), None));
    }
    fn push_limit(&self, reset_at: Option<i64>, raw: &str) {
        let hit = LimitHit {
            reset_at,
            raw: raw.into(),
        };
        self.0.lock().unwrap().0.push_back(out(1, raw, Some(hit)));
    }
    fn reqs(&self) -> Vec<AgentRequest> {
        self.0.lock().unwrap().1.clone()
    }
}
impl AgentRunner for QueueRunner {
    fn run(&self, req: &AgentRequest) -> PortResult<AgentOutcome> {
        let mut g = self.0.lock().unwrap();
        g.1.push(req.clone());
        Ok(g.0.pop_front().unwrap_or_else(|| out(0, "default", None)))
    }
}

struct StubInfra(PathBuf, String);
impl Clock for StubInfra {
    fn now(&self) -> i64 {
        1_000
    }
}
impl GitOps for StubInfra {
    fn task_worktree(&self, id: &str) -> PortResult<TaskWorktree> {
        let path = self.0.join(format!("wt-{id}"));
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
        Ok("diff --git a/src/new_widget.rs b/src/new_widget.rs\n+pub fn ok() {}\n".into())
    }
    fn integrate(&self, ids: &[String]) -> PortResult<Integration> {
        let tree = self.task_worktree("int")?;
        Ok(Integration {
            tree,
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
    fn merge(&self, _: &TaskWorktree, _: RegateFn<'_>) -> PortResult<MergeOutcome> {
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
        Ok(())
    }
    fn is_merged(&self, _: &str) -> PortResult<bool> {
        Ok(false)
    }
    fn cleanup(&self, _: &str) -> PortResult<()> {
        Ok(())
    }
}
impl GateRunner for StubInfra {
    fn run(&self, tree: &Path) -> PortResult<Receipt> {
        Ok(Receipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            repo_root: tree.to_string_lossy().into_owned(),
            tree_fp: "git-tree:ok".into(),
            head_sha: Some("h1".into()),
            base_sha: Some("base0".into()),
            mode: GateMode::Scoped,
            status: GateStatus::Clear,
            reasons: vec![],
            config_hash: self.1.clone(),
            commands: vec![],
            static_findings: vec![],
            suppressions: 0,
            head_lock: self.head_lock(tree)?,
            store_compliance: StoreComplianceReport::not_applicable(),
            created_at: 2_000,
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

fn emit(db: &Db, id: &str, kind: EventKind, body: Value, ts: i64) {
    db.emit(&FactoryEvent::new(kind, body.to_string(), ts).for_task(id))
        .unwrap();
}
fn out(code: i32, text: &str, limit_hit: Option<LimitHit>) -> AgentOutcome {
    let is_lim = limit_hit.is_some();
    AgentOutcome {
        exit_code: Some(code),
        timed_out: false,
        final_text: if is_lim { String::new() } else { text.into() },
        tokens_in: if code == 0 { 50 } else { 0 },
        tokens_out: if code == 0 { 25 } else { 0 },
        limit_hit,
        infra: None,
        log_tail: if is_lim { text.into() } else { String::new() },
    }
}
fn target(exec: &str, r: Arc<dyn AgentRunner>, pool: Arc<dyn AccountPool>) -> StageTarget {
    let mut t = StageTarget::new(exec, "", "", "", r, pool);
    t.hook_required = false;
    t
}

fn plan_v(summary: &str, file: &str, disp: Value, open_q: &[&str]) -> Value {
    json!({
        "summary": summary, "tier_assessment": "M", "tier_reasons": ["feature"],
        "files_to_change": [{"path": file, "responsibility": "impl", "risk": "low"}],
        "invariants_and_edge_cases": ["reject empty"],
        "red_green_tests": [{"ac_ref": "AC-1", "test_path": "tests/widget_test.rs",
                             "red_condition": "fails before", "green_condition": "passes after"}],
        "dispositions": disp, "open_questions": open_q
    })
}

fn crit_json(verdict: &str, issues: Value, fixed: &[&str]) -> String {
    let chk: Vec<_> = [
        "PLAN-AC-1",
        "PLAN-SCOPE",
        "PLAN-SEC",
        "PLAN-TEST",
        "PLAN-ROLLBACK",
    ]
    .map(|id| json!({"id": id, "pass": true, "evidence": "ok"}))
    .into();
    let res: Vec<_> = fixed
        .iter()
        .map(|id| json!({"issue_id": id, "status": "VERIFIED_FIXED", "reason": "ok"}))
        .collect();
    json!({"checklist": chk, "resolved_from_previous": res, "issues": issues, "verdict": verdict})
        .to_string()
}

fn p1(id: &str, cat: &str, loc: &str) -> Value {
    json!({"id": id, "severity": "P1", "category": cat, "location": loc, "description": "gap", "proposed_fix": "fix"})
}

fn assert_stalled(db: &Db, id: &str) {
    let task = db.get_task(id).unwrap();
    assert_eq!(task.state, TaskState::Blocked);
    assert!(matches!(
        &task.blocked_reason,
        Some(BlockedReason::NeedsHuman { why }) if why.starts_with(PLAN_DEBATE_STALLED_PREFIX)
    ));
}

struct World {
    _tmp: TempDir,
    db: Db,
    deps: Deps,
    router: StageRouter,
    author: Arc<QueueRunner>,
    chal: Arc<QueueRunner>,
    imp: Arc<QueueRunner>,
}

impl World {
    fn new(max_rounds: u32, tasks: &[(&str, Priority)]) -> Self {
        let tmp = TempDir::new().unwrap();
        let repo_dir = tmp.path().join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        let repo_root = repo_dir.to_string_lossy().into_owned();
        let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
        let spec = ImmutableSpec {
            description: "Add widget".into(),
            acceptance: vec!["works".into()],
        };
        for &(id, prio) in tasks {
            let mut t = Task::new_root(id, &repo_root, id, spec.clone(), prio, 100);
            t.state = TaskState::Ready;
            db.insert_task(&t).unwrap();
        }
        let cfg_dir = tmp.path().join("claude-1").to_string_lossy().into_owned();
        let acct = AccountProfile {
            id: "claude-1".into(),
            provider: ProviderKind::ClaudeCode,
            config_dir: cfg_dir,
            name: "claude-1".into(),
        };
        db.upsert_account(&acct).unwrap();
        let cp = Arc::new(StubPool(vec![acct]));
        let mut repo = RepoConfig::default();
        repo.run.executor = "claude".into();
        repo.pipeline.plan_debate = true;
        repo.pipeline.max_plan_rounds = max_rounds;
        repo.cross_audit.require_different_provider = true;
        let dir = tmp.path().to_path_buf();
        let infra = Arc::new(StubInfra(dir.clone(), aaf_engine::gate::config_hash(&repo)));
        let (author, chal, imp) = (
            Arc::<QueueRunner>::default(),
            Arc::<QueueRunner>::default(),
            Arc::<QueueRunner>::default(),
        );
        let router = StageRouter::new(target("claude", imp.clone(), cp.clone()))
            .with_plan_author(target("claude", author.clone(), cp.clone()))
            .with_plan_challenger(target(
                "antigravity",
                chal.clone(),
                Arc::new(StubPool(vec![])),
            ));
        let deps = Deps {
            store: Arc::new(db.clone()),
            clock: infra.clone(),
            runner: imp.clone(),
            accounts: cp,
            git: infra.clone(),
            gate: infra,
            notifier: Arc::new(WebhookNotifier { url: String::new() }),
            settings: Settings {
                repo_root,
                repo,
                max_parallel: 1,
                auto_merge: false,
                sandboxed: false,
                hook_required: false,
                agent_path: Default::default(),
                mobile: false,
                data_dir: dir.clone(),
                home: dir,
            },
        };
        Self {
            _tmp: tmp,
            db,
            deps,
            router,
            author,
            chal,
            imp,
        }
    }

    fn run_once(&self) -> RunSummary {
        let hooks = Hooks {
            idle: &|| false,
            reap: &|_| Ok(()),
        };
        run_with_router(&self.deps, None, Some(&self.router), Mode::Once, &hooks).unwrap()
    }
    fn counts(&self) -> (usize, usize, usize) {
        (
            self.author.reqs().len(),
            self.chal.reqs().len(),
            self.imp.reqs().len(),
        )
    }
}

#[test]
fn mode_once_runs_full_plan_debate_then_implements() {
    let w = World::new(3, &[("t1", Priority::P1)]);
    let disp = json!([{"issue_id":"I-1","action":"ACCEPTED","response":"SECRET_DISPOSITION","evidence":"updated"}]);
    let p_r1 = plan_v(
        "Initial plan",
        "src/new_widget.rs",
        json!([]),
        &["SECRET_OPEN_Q"],
    );
    let p_r2 = plan_v(
        "Hardened plan",
        "src/new_widget.rs",
        disp,
        &["SECRET_OPEN_Q"],
    );
    let i1 = json!([p1("I-1", "validation", "src/new_widget.rs:10")]);
    w.author.push(p_r1.to_string());
    w.author.push(p_r2.to_string());
    w.chal.push(crit_json("REVISE", i1, &[]));
    w.chal.push(crit_json("CONVERGED", json!([]), &["I-1"]));
    w.imp.push("implemented widget");

    let sum = w.run_once();
    assert_eq!(sum.review, vec!["t1"]);
    assert!(sum.blocked.is_empty() && sum.errors.is_empty() && sum.agent_runs == 1);
    assert_eq!(w.counts(), (2, 2, 1));

    let kinds = [EventKind::PlanDebateTurn, EventKind::PlanConverged];
    let mut evs =
        w.db.events_for_task_by_kinds("t1", &kinds, u32::MAX)
            .unwrap();
    evs.reverse();
    let seq: Vec<_> = evs
        .iter()
        .map(|e| {
            let v: Value = serde_json::from_str(&e.payload_json).unwrap();
            let (r, role) = (
                v["round"].as_u64().unwrap_or(0),
                v["role"].as_str().unwrap_or("-"),
            );
            format!("{}:{r}:{role}", e.kind.as_str())
        })
        .collect();
    assert_eq!(
        seq,
        [
            "plan_debate_turn:1:author",
            "plan_debate_turn:1:challenger",
            "plan_debate_turn:2:author",
            "plan_debate_turn:2:challenger",
            "plan_converged:2:-"
        ]
    );

    let prompt = &w.imp.reqs()[0].prompt;
    assert!(prompt.contains(PLAN_BEGIN) && prompt.contains(PLAN_END));
    assert!(!prompt.contains("SECRET_DISPOSITION") && !prompt.contains("SECRET_OPEN_Q"));
    let (_, after) = prompt.rsplit_once(PLAN_BEGIN).unwrap();
    let (inner, _) = after.split_once(PLAN_END).unwrap();
    let injected: PlanContract = serde_json::from_str(inner.trim()).unwrap();
    assert!(injected.dispositions.is_empty() && injected.open_questions.is_empty());
    assert_eq!(injected.summary, "Hardened plan");

    let runs = w.db.runs_for_task("t1").unwrap();
    assert_eq!(runs.len(), 5);
    assert!(
        runs[..4].iter().all(|r| r.executor == PLAN_EXECUTOR) && runs[4].executor != PLAN_EXECUTOR
    );
}

#[test]
fn plan_debate_stall_steer_unblock_starts_fresh_epoch() {
    let w = World::new(2, &[("t1", Priority::P1)]);
    let p = plan_v("Initial plan", "src/a.rs", json!([]), &[]).to_string();
    w.author.push(&p);
    w.author.push(&p);
    w.chal.push(crit_json(
        "REVISE",
        json!([p1("I-1", "security", "src/a.rs:5")]),
        &[],
    ));
    w.chal.push(crit_json(
        "REVISE",
        json!([p1("I-2", "correctness", "src/a.rs:12")]),
        &["I-1"],
    ));

    assert_eq!(w.run_once().blocked, vec!["t1"]);
    assert!(w.imp.reqs().is_empty());
    assert_stalled(&w.db, "t1");
    assert_eq!(
        w.db.last_account_of_task("t1", ProviderKind::ClaudeCode)
            .unwrap(),
        None
    );

    w.db.append_context_note("t1", "steer: bound retry loop to 3 attempts", 1_500)
        .unwrap();
    w.db.set_state("t1", TaskState::Ready, None, 1_501).unwrap();
    let unblock = json!({"from":"blocked","to":"ready","by":"human"});
    emit(&w.db, "t1", EventKind::TaskStateChanged, unblock, 1_501);

    w.author
        .push(plan_v("Steered plan", "src/a.rs", json!([]), &[]).to_string());
    w.chal.push(crit_json("CONVERGED", json!([]), &[]));
    w.imp.push("implemented steered plan");

    let s2 = w.run_once();
    assert_eq!(s2.review, vec!["t1"]);
    assert!(s2.blocked.is_empty() && w.imp.reqs().len() == 1);
    let fresh = &w.author.reqs()[2].prompt;
    assert!(fresh.contains("Round 1") && fresh.contains("bound retry loop to 3 attempts"));
    assert!(!fresh.contains("Previous Challenger Critique"));
}

#[test]
fn rate_limit_isolation_antigravity_429_does_not_block_claude() {
    let tasks = [
        ("t-a", Priority::P0),
        ("t-b", Priority::P1),
        ("t-c", Priority::P2),
    ];
    let w = World::new(3, &tasks);
    let pv = plan_v("Ready plan", "src/a.rs", json!([]), &[]);
    let pc: PlanContract = serde_json::from_value(pv.clone()).unwrap();
    let hash = canonical_plan_hash(&pc);
    w.author.push(pv.to_string());
    w.chal.push_limit(Some(10_000), "429 RESOURCE_EXHAUSTED");
    w.imp.push("implemented t-b");

    let turn = json!({"round":1,"role":"author","ok":true,"summary":pc.summary,"plan_contract_hash":hash,"plan_contract":pc});
    let conv = json!({"summary":pc.summary,"plan_contract_hash":hash,"plan_contract":pc});
    emit(&w.db, "t-a", EventKind::PlanDebateTurn, turn, 900);
    emit(&w.db, "t-b", EventKind::PlanConverged, conv, 901);

    let sum = w.run_once();
    assert_eq!(w.counts(), (1, 1, 1));
    assert_eq!(sum.review, vec!["t-b"]);
    assert_eq!(w.db.get_task("t-a").unwrap().state, TaskState::Ready);
    assert_eq!(w.db.get_task("t-c").unwrap().state, TaskState::Ready);
    assert!(sum.blocked.is_empty());
}

#[test]
fn plan_turn_format_failure_blocks_stalled_never_calls_t4_decompose() {
    let w = World::new(3, &[("t1", Priority::P1)]);
    let total = (MAX_CONSECUTIVE_TURN_FAILS as usize) * 2;
    for i in 0..total {
        w.author.push(format!("not valid json #{i}"));
    }

    assert_eq!(w.run_once().blocked, vec!["t1"]);
    assert_eq!(w.author.reqs().len(), total);
    assert!(
        w.author.reqs()[1]
            .prompt
            .contains("failed strict JSON validation")
    );
    assert!(w.chal.reqs().is_empty() && w.imp.reqs().is_empty());
    assert_stalled(&w.db, "t1");
    let root = w.deps.settings.repo_root.as_str();
    assert_eq!(w.db.list_tasks(Some(root), None).unwrap().len(), 1);
    assert!(
        w.db.events_for_task_by_kinds("t1", &[EventKind::UnblockStep], u32::MAX)
            .unwrap()
            .is_empty()
    );
    assert_eq!(w.db.get_meta(META_CONSECUTIVE_ROOT_FAILS).unwrap(), None);
}

#[test]
fn antigravity_locked_pool_enforces_exclusive_lock_even_with_zero_accounts() {
    let tmp = TempDir::new().unwrap();
    let inner = Arc::new(StubPool(vec![]));
    assert_eq!(inner.provider_account_count().unwrap(), 0);
    let pool = AntigravityLockedPool::new(inner, tmp.path().to_path_buf());

    let grant1 = pool.acquire_for("t-a", Purpose::Work).unwrap();
    let lock_file = tmp.path().join("leases/provider-antigravity.lock");
    assert!(grant1.is_some() && lock_file.is_file());
    assert!(pool.acquire_for("t-b", Purpose::Work).unwrap().is_none());
    assert!(pool.acquire().unwrap().is_none());

    drop(grant1);

    let grant2 = pool.acquire_for("t-b", Purpose::Work).unwrap();
    assert!(grant2.is_some());
    assert!(pool.acquire_for("t-c", Purpose::Audit).unwrap().is_none());
    drop(grant2);
}
