#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Wave 3 Tiered Routing, Tier-S Fail-Closed Debate Skip, Auto-Escalation,
//! and Per-Stage `hook_required` integration tests (`docs/PLAN_DUAL_FRONTIER.md` §D.3.2, §F Wave 3).

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
use aaf_domain::pipeline::{PlanContract, ReviewerConfig, TaskTier};
use aaf_domain::ports::{
    AccountStore, AgentOutcome, AgentRequest, AgentRunner, Clock, EventSink, LimitHit, PortResult,
    RunStore, TaskStore,
};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::factory::adapters::WebhookNotifier;
use aaf_engine::factory::{
    AccountPool, Deps, GateRunner, GitOps, Grant, Hooks, Integration, Mode, RunSummary, Settings,
    StageRouter, StageTarget, any_stage_uses_claude, classify_tier, is_high_risk_path,
    is_tier_s_skip_debate, rework_count, run_with_router,
};
use aaf_engine::git::{MergeOutcome, PrOutcome, TaskWorktree};
use aaf_engine::plan_debate::canonical_plan_hash;
use aaf_engine::unblocker::QuotaRoute;
use serde_json::{Value, json};
use tempfile::TempDir;

type RegateFn<'a> = &'a dyn Fn(&Path) -> Result<bool, String>;
type Wt = TaskWorktree;

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
struct QueueRunner(Mutex<(VecDeque<String>, Vec<AgentRequest>)>);
impl QueueRunner {
    fn push(&self, text: impl Into<String>) {
        self.0.lock().unwrap().0.push_back(text.into());
    }
    fn reqs(&self) -> Vec<AgentRequest> {
        self.0.lock().unwrap().1.clone()
    }
}
impl AgentRunner for QueueRunner {
    fn run(&self, req: &AgentRequest) -> PortResult<AgentOutcome> {
        let mut g = self.0.lock().unwrap();
        g.1.push(req.clone());
        Ok(AgentOutcome {
            exit_code: Some(0),
            timed_out: false,
            final_text: g.0.pop_front().unwrap_or_else(|| "ok".into()),
            tokens_in: 40,
            tokens_out: 20,
            limit_hit: None,
            infra: None,
            log_tail: String::new(),
        })
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
        Ok("diff --git a/src/w.rs b/src/w.rs\n+pub fn ok() {}\n".into())
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
            new_base_sha: "b1".into(),
        })
    }
    fn open_pr(&self, _: &Wt, _: &str, _: &Receipt, _: Option<&str>) -> PortResult<PrOutcome> {
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

fn stage(exec: &str, model: &str, r: Arc<QueueRunner>, p: Arc<dyn AccountPool>) -> StageTarget {
    StageTarget::new(exec, "", model, "", r, p)
}
fn mk_task(id: &str, prio: Priority, scope: &[&str]) -> Task {
    let spec = ImmutableSpec {
        description: "Work".into(),
        acceptance: vec!["ok".into()],
    };
    let mut t = Task::new_root(id, "/tmp/repo", id, spec, prio, 100);
    t.state = TaskState::Ready;
    t.scope_paths = scope.iter().map(|s| (*s).to_string()).collect();
    t
}
fn mk_plan(tier: &str, files: &[&str]) -> PlanContract {
    let f: Vec<_> = files
        .iter()
        .map(|p| json!({"path": p, "responsibility": "e"}))
        .collect();
    serde_json::from_value(json!({
        "summary": "P",
        "tier_assessment": tier,
        "files_to_change": f,
        "red_green_tests": [{"ac_ref": "AC-1", "test_path": "tests/t.rs", "red_condition": "r", "green_condition": "g"}]
    }))
    .unwrap()
}
fn crit_converged() -> String {
    let chk: Vec<Value> = "PLAN-AC-1,PLAN-SCOPE,PLAN-SEC,PLAN-TEST,PLAN-ROLLBACK"
        .split(',')
        .map(|id| json!({"id": id, "pass": true, "evidence": "ok"}))
        .collect();
    json!({"checklist": chk, "resolved_from_previous": [], "issues": [], "verdict": "CONVERGED"})
        .to_string()
}
fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

struct World {
    _tmp: TempDir,
    db: Db,
    deps: Deps,
    router: StageRouter,
    debate: (Arc<QueueRunner>, Arc<QueueRunner>),
    imp: Arc<QueueRunner>,
    esc: Arc<QueueRunner>,
}

impl World {
    fn new(repo: RepoConfig, tasks: &[(&str, Priority, &[&str])]) -> Self {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();
        let repo_root = dir.join("repo").to_string_lossy().into_owned();
        std::fs::create_dir_all(&repo_root).unwrap();
        let db = Db::open(&dir.join("aaf.db")).unwrap();
        for &(id, prio, scope) in tasks {
            let mut t = mk_task(id, prio, scope);
            t.repo_root = repo_root.clone();
            db.insert_task(&t).unwrap();
        }
        let acct = AccountProfile {
            id: "claude-1".into(),
            provider: ProviderKind::ClaudeCode,
            config_dir: dir.join("claude-1").to_string_lossy().into_owned(),
            name: "claude-1".into(),
        };
        db.upsert_account(&acct).unwrap();
        let cp: Arc<dyn AccountPool> = Arc::new(StubPool(vec![acct]));
        let ap: Arc<dyn AccountPool> = Arc::new(StubPool(vec![]));
        let infra = Arc::new(StubInfra(dir.clone(), aaf_engine::gate::config_hash(&repo)));
        let [author, chal, imp, esc] = std::array::from_fn(|_| Arc::<QueueRunner>::default());
        let no_hook = |e, m, r: &Arc<QueueRunner>, p: &Arc<dyn AccountPool>| {
            let mut t = stage(e, m, r.clone(), p.clone());
            t.hook_required = false;
            t
        };
        let router = StageRouter::new(no_hook("claude", "std", &imp, &cp))
            .with_implementer(no_hook("claude", "std", &imp, &cp))
            .with_implementer_escalated(no_hook("claude", "esc", &esc, &cp))
            .with_plan_author(no_hook("claude", "auth", &author, &cp))
            .with_plan_challenger(no_hook("antigravity", "chal", &chal, &ap));
        let settings = Settings {
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
            owner_github_allowlist: Vec::new(),
        };
        let deps = Deps {
            store: Arc::new(db.clone()),
            clock: infra.clone(),
            runner: imp.clone(),
            accounts: cp,
            git: infra.clone(),
            gate: infra,
            notifier: Arc::new(WebhookNotifier { url: String::new() }),
            settings,
        };
        Self {
            _tmp: tmp,
            db,
            deps,
            router,
            debate: (author, chal),
            imp,
            esc,
        }
    }
    fn run_once(&self) -> RunSummary {
        let h = Hooks {
            idle: &|| false,
            reap: &|_| Ok(()),
        };
        run_with_router(&self.deps, None, Some(&self.router), Mode::Once, &h).unwrap()
    }
    fn note(&self, id: &str, text: &str) {
        self.db.append_context_note(id, text, 200).unwrap();
    }
    fn esc_events(&self, id: &str) -> Vec<FactoryEvent> {
        let k = [EventKind::ImplementerEscalated];
        self.db.events_for_task_by_kinds(id, &k, u32::MAX).unwrap()
    }
    fn check(&self, tk: &Task, pl: Option<&PlanContract>, exp: (&str, TaskTier, bool)) {
        let (t, tier, esc) = self
            .router
            .resolve_implementer(tk, pl, &self.deps.settings.repo);
        assert_eq!((t.model.as_str(), tier, esc), exp, "task={}", tk.id);
    }
    fn check_id(&self, id: &str, exp: (&str, TaskTier, bool)) {
        self.check(&self.db.get_task(id).unwrap(), None, exp);
    }
}

#[test]
fn tier_s_skip_debate_requires_all_four_conditions_and_fails_closed() {
    let mut cfg = RepoConfig::default();
    cfg.pipeline.plan_debate = true;
    cfg.pipeline.allow_tier_s_skip_debate = true;
    cfg.cross_audit.require_different_provider = true;
    cfg.gate.docs_fast_path_exclude_globs = vec!["docs/SECURITY.md".into()];
    let skip = |p, s: &[&str], c: &RepoConfig| is_tier_s_skip_debate(&mk_task("t", p, s), c);

    // Passes ONLY when all 4 conditions hold.
    let docs_p1 = mk_task("t-docs-p1", Priority::P1, &["docs/guide.md", "README.md"]);
    assert!(is_tier_s_skip_debate(&docs_p1, &cfg));
    assert!(skip(Priority::P2, &["docs/faq.rst"], &cfg));
    assert_eq!(classify_tier(&docs_p1, None, &cfg), TaskTier::S);

    // Fails closed if any 1 of the 4 conditions fails.
    let mut disabled = cfg.clone();
    disabled.pipeline.allow_tier_s_skip_debate = false;
    assert!(!skip(Priority::P1, &["docs/guide.md"], &disabled));
    assert!(!skip(Priority::P0, &["docs/guide.md"], &cfg));
    assert!(!skip(Priority::P1, &[], &cfg));
    for bad in [
        &["docs/guide.md", "src/lib.rs"][..],
        &["docs/SECURITY.md"][..],
        &["CLAUDE.md"][..],
        &["../README.md"][..],
    ] {
        assert!(!skip(Priority::P1, bad, &cfg), "{bad:?}");
    }
    let mut locked = cfg.clone();
    locked.gate.docs_fast_path_exclude_globs.clear();
    locked.gate.head_lock_globs.push("docs/SECURITY.md".into());
    assert!(!skip(Priority::P1, &["docs/SECURITY.md"], &locked));

    // End-to-end: Tier S docs skips debate; P0 docs still debates & routes to escalated.
    let w = World::new(
        cfg,
        &[
            ("t-docs", Priority::P1, &["docs/guide.md"]),
            ("t-p0-docs", Priority::P0, &["docs/guide.md"]),
        ],
    );
    let p_json = serde_json::to_string(&mk_plan("M", &["docs/guide.md"])).unwrap();
    w.debate.0.push(p_json);
    w.debate.1.push(crit_converged());

    let sum = w.run_once();
    assert_eq!(sorted(sum.review), vec!["t-docs", "t-p0-docs"]);
    assert!(sum.blocked.is_empty() && sum.errors.is_empty());
    assert_eq!((w.debate.0.reqs().len(), w.debate.1.reqs().len()), (1, 1));
    assert_eq!((w.imp.reqs().len(), w.esc.reqs().len()), (1, 1));
    assert!(w.imp.reqs()[0].cwd.ends_with("wt-t-docs"));
    assert!(w.esc.reqs()[0].cwd.ends_with("wt-t-p0-docs"));
}

#[test]
fn tier_m_rework_count_ignores_t2_and_resets_after_rework_or_unblocked() {
    let mut t = mk_task("t-cnt", Priority::P1, &["src/lib.rs"]);
    let steps: &[(&[&str], u32)] = &[
        (&[], 0),
        // T2 infra/flaky reruns never increment rework_count (R3-CONC-7).
        (&["unblock[T2]: timeout", "unblock[T2]: flaky"], 0),
        (&["audit_rework: fix P1"], 1),
        (&["unblock[T3] patch test"], 2),
        // "rework:" and "unblocked:" each reset the counting window.
        (&["rework: reset", "unblock[T2]: infra"], 0),
        (&["audit_rework: cycle 2"], 1),
        (&["unblocked: resumed", "unblock[T2]: flake"], 0),
        (&["unblock[T3] compile fix"], 1),
    ];
    for &(notes, expected) in steps {
        t.context_notes.extend(notes.iter().map(|s| (*s).into()));
        assert_eq!(rework_count(&t), expected, "notes={notes:?}");
    }
}

#[test]
fn tier_m_auto_escalates_on_audit_rework_or_t3_not_t2_and_emits_event() {
    let mut cfg = RepoConfig::default();
    cfg.pipeline.plan_debate = false;
    cfg.pipeline.escalate_implementer_after_reworks = 1;
    let w = World::new(
        cfg,
        &[
            ("t-t2", Priority::P1, &["src/a.rs"]),
            ("t-audit", Priority::P1, &["src/b.rs"]),
            ("t-t3", Priority::P1, &["src/c.rs"]),
        ],
    );
    w.note("t-t2", "unblock[T2]: flaky runner");
    w.note("t-audit", "audit_rework: P1 edge case");
    w.note("t-t3", "unblock[T3] failing assertion");

    w.check_id("t-t2", ("std", TaskTier::M, false));
    w.check_id("t-audit", ("esc", TaskTier::M, true));
    w.check_id("t-t3", ("esc", TaskTier::M, true));

    let sum = w.run_once();
    assert_eq!(sorted(sum.review), vec!["t-audit", "t-t2", "t-t3"]);
    assert!(sum.blocked.is_empty() && sum.errors.is_empty());
    assert_eq!(w.imp.reqs().len(), 1);
    assert!(w.imp.reqs()[0].cwd.ends_with("wt-t-t2"));
    let cwds: Vec<_> = w.esc.reqs().into_iter().map(|r| r.cwd).collect();
    assert_eq!(cwds.len(), 2);
    assert!(cwds.iter().any(|c| c.ends_with("wt-t-audit")));
    assert!(cwds.iter().any(|c| c.ends_with("wt-t-t3")));

    assert!(w.esc_events("t-t2").is_empty());
    for id in ["t-audit", "t-t3"] {
        let evs = w.esc_events(id);
        assert_eq!(evs.len(), 1, "expected ImplementerEscalated for {id}");
        assert!(serde_json::from_str::<Value>(&evs[0].payload_json).is_ok());
    }
}

#[test]
fn tier_l_routes_directly_to_escalated_implementer_from_turn_1() {
    let mut cfg = RepoConfig::default();
    cfg.pipeline.plan_debate = false;
    let w = World::new(
        cfg.clone(),
        &[
            ("t-p0", Priority::P0, &["src/widget.rs"]),
            ("t-auth", Priority::P1, &["src/auth/login.rs"]),
        ],
    );
    let hl = &cfg.gate.head_lock_globs;

    // 1. P0 -> Tier L (rework_count == 0); 2. tier_assessment == "L"; 3. > 5 files.
    let p0 = w.db.get_task("t-p0").unwrap();
    assert_eq!(rework_count(&p0), 0);
    w.check(&p0, None, ("esc", TaskTier::L, false));
    let p1 = mk_task("t-p1", Priority::P1, &["src/widget.rs"]);
    let plan_l = mk_plan("L", &["src/widget.rs"]);
    w.check(&p1, Some(&plan_l), ("esc", TaskTier::L, false));
    let f5 = ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs"];
    let f6 = ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs", "f.rs"];
    let tier = |f: &[&str]| classify_tier(&p1, Some(&mk_plan("M", f)), &cfg);
    assert_eq!((tier(&f5), tier(&f6)), (TaskTier::M, TaskTier::L));

    // 4. head_lock_globs, AGENT_CONFIG_GLOBS, and high-risk segments -> Tier L from Turn 1.
    let risky = [
        "Cargo.toml",
        ".github/workflows/ci.yml",
        ".claude/settings.json",
        "./.claude/settings.json",
        "src/executors/sandbox.rs",
        "src/auth/login.rs",
        "db/migration_01.sql",
    ];
    for path in risky {
        assert!(is_high_risk_path(path, hl), "{path}");
        w.check(
            &p1,
            Some(&mk_plan("M", &[path])),
            ("esc", TaskTier::L, false),
        );
    }
    assert!(!is_high_risk_path("src/widget.rs", hl));

    // End-to-end: P0 & high-risk-plan P1 both run on escalated runner from Turn 1.
    let ap = mk_plan("M", &["src/auth/login.rs"]);
    let h = canonical_plan_hash(&ap);
    let conv = json!({"summary": ap.summary, "plan_contract_hash": h, "plan_contract": ap});
    let ev = FactoryEvent::new(EventKind::PlanConverged, conv.to_string(), 900).for_task("t-auth");
    w.db.emit(&ev).unwrap();

    let sum = w.run_once();
    assert_eq!(sorted(sum.review), vec!["t-auth", "t-p0"]);
    assert!(w.imp.reqs().is_empty() && w.esc.reqs().len() == 2);
    assert!(w.esc_events("t-p0").is_empty() && w.esc_events("t-auth").is_empty());
}

#[test]
fn per_stage_hook_required_blocks_claude_implementer_under_antigravity_default() {
    let mut cfg = RepoConfig::default();
    cfg.run.executor = "antigravity".into();
    cfg.pipeline.plan_debate = false;
    assert!(!any_stage_uses_claude(&cfg));
    cfg.pipeline.implementer.executor = "claude".into();
    assert!(any_stage_uses_claude(&cfg));

    let mut rev_cfg = cfg.clone();
    rev_cfg.pipeline.implementer.executor.clear();
    rev_cfg.cross_audit.reviewers.push(ReviewerConfig {
        id: "r1".into(),
        executor: "claude".into(),
        ..Default::default()
    });
    assert!(any_stage_uses_claude(&rev_cfg));

    // End-to-end (R3-SEC-3): [run] = antigravity (Settings.hook_required = false),
    // [pipeline.implementer] = claude (StageTarget.hook_required = true),
    // worktree lacks `.claude/settings.json` -> blocks fail-closed without running Claude.
    let mut w = World::new(cfg, &[("t-hook", Priority::P1, &["src/widget.rs"])]);
    assert!(!w.deps.settings.hook_required);
    let claude_imp = stage("claude", "std", w.imp.clone(), Arc::clone(&w.deps.accounts));
    let ag_def = stage("antigravity", "", w.esc.clone(), Arc::new(StubPool(vec![])));
    assert!(claude_imp.hook_required && !ag_def.hook_required);
    w.router = StageRouter::new(ag_def).with_implementer(claude_imp);

    let sum = w.run_once();
    assert_eq!(sum.blocked, vec!["t-hook"]);
    assert!(sum.review.is_empty() && w.imp.reqs().is_empty());
    assert!(w.db.runs_for_task("t-hook").unwrap().is_empty());
    let task = w.db.get_task("t-hook").unwrap();
    assert_eq!(task.state, TaskState::Blocked);
    assert!(matches!(
        &task.blocked_reason,
        Some(BlockedReason::NeedsHuman { why })
            if why.contains(".claude/settings.json") && why.contains("does not exist")
    ));
}
