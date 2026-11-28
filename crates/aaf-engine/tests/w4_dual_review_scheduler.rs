#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Wave 4 Blind Parallel Dual-Review Scheduler, Crash Recovery, Partial Cache,
//! PreGateAudit Guard, and Adjudication integration tests (`docs/PLAN_DUAL_FRONTIER.md` §F).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use aaf_domain::account::AccountProfile;
use aaf_domain::blocked::BlockedReason;
use aaf_domain::budget::RootBudget;
use aaf_domain::config::RepoConfig;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::gate::*;
use aaf_domain::pipeline::PlanContract;
use aaf_domain::ports::*;
use aaf_domain::task::{Priority, Task, TaskState};
use aaf_engine::cross_audit::GOLDEN_IDS;
use aaf_engine::db::Db;
use aaf_engine::factory::adapters::WebhookNotifier;
use aaf_engine::factory::*;
use aaf_engine::git::{MergeOutcome, PrOutcome, TaskWorktree as Wt};
use aaf_engine::handover::load_handover;
use aaf_engine::plan_debate::canonical_plan_hash;
use aaf_engine::unblocker::QuotaRoute;
use aaf_fp::{PartialAuditKey, PartialAuditRecord};
use serde_json::{Value, json};
use tempfile::TempDir;

type RegateFn<'a> = &'a dyn Fn(&Path) -> Result<bool, String>;
type SyncGate = Arc<(Mutex<(usize, usize, usize)>, Condvar)>;

const REV_A: (&str, &str, &str) = ("antigravity-neon", "antigravity", "neon-test");
const REV_B: (&str, &str, &str) = ("claude-opus", "claude", "opus-test");
const TREE_FP: &str = "git-tree:ok";
const BASE_SHA: &str = "base0";
const DIFF: &str = "diff --git a/src/lib.rs b/src/lib.rs\n--- /dev/null\n+++ b/src/lib.rs\n\
                    @@ -0,0 +1,2 @@\n+pub fn ok() {}\n+pub fn helper() -> u32 { 42 }\n";

fn sync_two(g: &SyncGate) {
    let mut s = g.0.lock().unwrap();
    (s.0, s.1, s.2) = (s.0 + 1, s.1.max(s.0 + 1), s.2 + 1);
    if s.2 >= 2 {
        g.1.notify_all();
    } else {
        let w =
            g.1.wait_timeout_while(s, Duration::from_secs(5), |x| x.2 < 2);
        s = w.unwrap().0;
    }
    s.0 -= 1;
}

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
struct QueueRunner(Mutex<(VecDeque<AgentOutcome>, Vec<AgentRequest>, Option<SyncGate>)>);
impl QueueRunner {
    fn push(&self, text: impl AsRef<str>) {
        let o = out(0, text.as_ref(), None);
        self.0.lock().unwrap().0.push_back(o);
    }
    fn push_limit(&self, reset_at: Option<i64>, raw: &str) {
        let o = out(
            1,
            raw,
            Some(LimitHit {
                reset_at,
                raw: raw.into(),
            }),
        );
        self.0.lock().unwrap().0.push_back(o);
    }
    fn push_oauth_refreshed(&self, raw: &str) {
        let mut o = out(1, raw, None);
        o.infra = Some(aaf_domain::ports::InfraFailure {
            kind: aaf_domain::ports::InfraKind::OAuthRefreshed,
            raw: raw.into(),
        });
        self.0.lock().unwrap().0.push_back(o);
    }
    fn arm(&self, gate: SyncGate) {
        self.0.lock().unwrap().2 = Some(gate);
    }
    fn reqs(&self) -> Vec<AgentRequest> {
        self.0.lock().unwrap().1.clone()
    }
}
impl AgentRunner for QueueRunner {
    fn run(&self, req: &AgentRequest) -> PortResult<AgentOutcome> {
        if let Some(g) = self.0.lock().unwrap().2.take() {
            sync_two(&g);
        }
        let mut g = self.0.lock().unwrap();
        g.1.push(req.clone());
        Ok(g.0.pop_front().unwrap_or_else(|| out(0, "default", None)))
    }
}

struct StubInfra(PathBuf, String, AtomicBool);
impl Clock for StubInfra {
    fn now(&self) -> i64 {
        1_000
    }
}
impl GitOps for StubInfra {
    fn task_worktree(&self, id: &str) -> PortResult<Wt> {
        let path = self.0.join(format!("wt-{id}"));
        std::fs::create_dir_all(&path).unwrap();
        Ok(Wt {
            path,
            branch: format!("aaf/{id}"),
        })
    }
    fn commit_all(&self, _: &Wt, _: &str) -> PortResult<()> {
        Ok(())
    }
    fn has_changes(&self, _: &Wt) -> PortResult<bool> {
        Ok(true)
    }
    fn diff(&self, _: &Wt) -> PortResult<String> {
        Ok(DIFF.into())
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
    fn rebase(&self, _: &Wt) -> PortResult<Vec<String>> {
        Ok(vec![])
    }
    fn base_sha(&self) -> PortResult<String> {
        Ok(BASE_SHA.into())
    }
    fn merge(&self, _: &Wt, _: RegateFn<'_>) -> PortResult<MergeOutcome> {
        Ok(MergeOutcome::Merged {
            new_base_sha: "base1".into(),
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
        Ok(from_j(json!({
            "schema_version": RECEIPT_SCHEMA_VERSION, "repo_root": tree.to_string_lossy(),
            "tree_fp": TREE_FP, "head_sha": "h1", "base_sha": BASE_SHA, "mode": "scoped",
            "status": "CLEAR", "reasons": [], "config_hash": self.1, "commands": [],
            "static_findings": [], "suppressions": 0, "created_at": 2_000,
            "head_lock": {"status": "CLEAR", "lock_fp": "", "locked_changes": [], "approval": null},
            "store_compliance": StoreComplianceReport::not_applicable()
        })))
    }
    fn head_lock(&self, _: &Path) -> PortResult<HeadLockReport> {
        let v = if self.2.swap(false, Ordering::SeqCst) {
            json!({"status": "BLOCKED", "lock_fp": "lock-fp-1", "approval": null,
                   "locked_changes": [{"path": ".github/workflows/ci.yml", "change": "modified", "tier": "hard"}]})
        } else {
            json!({"status": "CLEAR", "lock_fp": "", "locked_changes": [], "approval": null})
        };
        Ok(from_j(v))
    }
    fn tree_binding(&self, _: &Path) -> Option<(String, Option<String>)> {
        Some((TREE_FP.into(), Some(BASE_SHA.into())))
    }
}

fn from_j<T: serde::de::DeserializeOwned>(v: Value) -> T {
    serde_json::from_value(v).unwrap()
}
fn out(code: i32, text: &str, limit_hit: Option<LimitHit>) -> AgentOutcome {
    let lim = limit_hit.is_some();
    AgentOutcome {
        exit_code: Some(code),
        timed_out: false,
        final_text: if lim { String::new() } else { text.into() },
        tokens_in: if code == 0 { 50 } else { 0 },
        tokens_out: if code == 0 { 25 } else { 0 },
        limit_hit,
        infra: None,
        log_tail: if lim { text.into() } else { String::new() },
    }
}
fn scores(score: u8, why: &str) -> Value {
    let r = if score < 100 { why } else { "" };
    let f = |id| json!({"id": id, "score": score, "deduction_reason": r});
    GOLDEN_IDS.iter().map(f).collect()
}
fn answers(yes: &[(&str, &str)]) -> Value {
    let base = "TEST-BRANCH HARDCODED SECRET ASSERT GATE-CI STUB AC-1".split(' ');
    let has_p = yes.iter().any(|(k, _)| *k == "PLAN-DRIFT");
    let map = |id| {
        let ev = yes.iter().find(|(k, _)| *k == id).map(|(_, e)| *e);
        json!({"id": id, "answer": if ev.is_some() { "yes" } else { "no" }, "evidence": ev.unwrap_or("")})
    };
    base.chain(has_p.then_some("PLAN-DRIFT")).map(map).collect()
}
fn dual(yes: &[(&str, &str)], issues: Value, score: u8, why: &str) -> String {
    json!({"answers": answers(yes), "issues": issues, "scores": scores(score, why)}).to_string()
}
fn pass_dual(score: u8, why: &str) -> String {
    dual(&[], json!([]), score, why)
}
fn issue(id: &str, sev: &str, cat: &str, loc: &str) -> Value {
    json!([{"id": id, "severity": sev, "category": cat, "location": loc, "description": "defect", "proposed_fix": "fix"}])
}
fn adj(i: &str, d: &str, e: &str, r: &str) -> String {
    json!({"verdicts": [{"issue_id": i, "decision": d, "evidence": e, "reason": r}]}).to_string()
}

struct World {
    _tmp: TempDir,
    db: Db,
    deps: Deps,
    router: StageRouter,
    imp: Arc<QueueRunner>,
    rev_a: Arc<QueueRunner>,
    rev_b: Arc<QueueRunner>,
    cfg_hash: String,
}

impl World {
    fn new(lock_first: bool, id: &str, state: TaskState) -> Self {
        let tmp = TempDir::new().unwrap();
        let repo_dir = tmp.path().join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        let repo_root = repo_dir.to_string_lossy().into_owned();
        let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
        let spec = from_j(json!({"description": "Add lib", "acceptance": ["works"]}));
        let mut t = Task::new_root(id, &repo_root, id, spec, Priority::P1, 100);
        t.state = state;
        db.insert_task(&t).unwrap();
        let c1 = tmp.path().join("c1").to_string_lossy().into_owned();
        let acct = from_j(
            json!({"id": "claude-1", "provider": "claude_code", "config_dir": c1, "name": "claude-1"}),
        );
        db.upsert_account(&acct).unwrap();
        let (cp, ap) = (Arc::new(StubPool(vec![acct])), Arc::new(StubPool(vec![])));
        let repo: RepoConfig = from_j(json!({
            "run": {"executor": "claude"},
            "cross_audit": {
                "audit_scope": "always", "dual_review": true, "cross_adjudicate": true,
                "require_different_provider": true, "score_gate": true,
                "min_average_score": 95, "min_criterion_score": 90,
                "reviewers": [
                    {"id": REV_A.0, "executor": REV_A.1, "model": REV_A.2, "effort": "high"},
                    {"id": REV_B.0, "executor": REV_B.1, "model": REV_B.2, "effort": "high"}
                ]
            }
        }));
        let cfg_hash = aaf_engine::gate::config_hash(&repo);
        let dir = tmp.path().to_path_buf();
        let lock = AtomicBool::new(lock_first);
        let infra = Arc::new(StubInfra(dir.clone(), cfg_hash.clone(), lock));
        let (imp, rev_a, rev_b): (Arc<QueueRunner>, Arc<QueueRunner>, Arc<QueueRunner>) =
            Default::default();
        let mk = |e: &str, m: &str, r: Arc<dyn AgentRunner>, p: Arc<dyn AccountPool>| {
            let mut st = StageTarget::new(e, "", m, "high", r, p);
            st.hook_required = false;
            st
        };
        let a_t = mk(REV_A.1, REV_A.2, rev_a.clone(), ap);
        let b_t = mk(REV_B.1, REV_B.2, rev_b.clone(), cp.clone());
        let router = StageRouter::new(mk("claude", "", imp.clone(), cp.clone()))
            .with_reviewers(vec![(REV_A.0.into(), a_t), (REV_B.0.into(), b_t)]);
        let settings = Settings {
            repo_root,
            repo,
            max_parallel: 2,
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
            imp,
            rev_a,
            rev_b,
            cfg_hash,
        }
    }
    fn run_once(&self, audit: Option<&AuditBackend>) -> RunSummary {
        let hooks = Hooks {
            idle: &|| false,
            reap: &|_| Ok(()),
        };
        run_with_router(&self.deps, audit, Some(&self.router), Mode::Once, &hooks).unwrap()
    }
    fn wt(&self, id: &str) -> PathBuf {
        self.deps.settings.data_dir.join(format!("wt-{id}"))
    }
    fn verdict(&self, id: &str) -> Option<PostGateAuditRecord> {
        let d = &self.deps.settings.data_dir;
        let k = aaf_fp::load_key(d).ok()?;
        aaf_fp::read_audit_verdict(d, &k, &self.wt(id), TREE_FP, &self.cfg_hash).unwrap()
    }
    fn events(&self, id: &str, kinds: &[EventKind]) -> Vec<FactoryEvent> {
        let evs = self.db.events_for_task_by_kinds(id, kinds, u32::MAX);
        evs.unwrap()
    }
    fn assert_review(&self, id: &str, sum: &RunSummary, adjudicated: bool) -> PostGateAuditRecord {
        assert_eq!(sum.review, vec![id]);
        assert!(sum.blocked.is_empty() && sum.errors.is_empty());
        assert_eq!(self.db.get_task(id).unwrap().state, TaskState::Review);
        let rec = self.verdict(id).expect("signed PostGateAuditRecord");
        assert!(rec.dual_consensus && rec.adjudicated == adjudicated && rec.reviewers.len() == 2);
        rec
    }
    fn assert_rework(&self, id: &str, sum: &RunSummary, needle: &str) {
        assert!(sum.review.is_empty() && sum.blocked.is_empty() && sum.errors.is_empty());
        let ca = self.events(id, &[EventKind::CrossAudit]);
        assert_eq!(ca.len(), 1);
        let v: Value = serde_json::from_str(&ca[0].payload_json).unwrap();
        assert!(v["verdict"].as_str().unwrap().starts_with("fail:"));
        let t = self.db.get_task(id).unwrap();
        assert_eq!(t.state, TaskState::Ready);
        let ok = t
            .context_notes
            .iter()
            .any(|n| n.starts_with("audit_rework:") && n.contains(needle));
        assert!(ok);
    }
}

#[test]
fn dual_review_parallel_both_pass_signs_consensus_and_reaches_review() {
    let w = World::new(false, "t1", TaskState::Ready);
    let sync = SyncGate::default();
    w.rev_a.arm(sync.clone());
    w.rev_b.arm(sync.clone());
    w.imp.push("implemented lib");
    w.rev_a.push(pass_dual(96, "REV_A_SECRET_96"));
    w.rev_b.push(pass_dual(98, "REV_B_SECRET_98"));

    let sum = w.run_once(None);
    let rec = w.assert_review("t1", &sum, false);
    assert_eq!(sync.0.lock().unwrap().1, 2);
    assert_eq!((w.rev_a.reqs().len(), w.rev_b.reqs().len()), (1, 1));
    assert!(!w.rev_a.reqs()[0].prompt.contains("REV_B_SECRET_98"));
    assert!(!w.rev_b.reqs()[0].prompt.contains("REV_A_SECRET_96"));
    assert_eq!((rec.llm_average, rec.effective_score), (Some(96), 96));
    assert_eq!(json!(rec.scores), scores(96, "REV_A_SECRET_96"));

    let ca = w.events("t1", &[EventKind::CrossAudit]);
    assert_eq!(ca.len(), 1);
    let ca_v: Value = serde_json::from_str(&ca[0].payload_json).unwrap();
    assert!(ca_v["verdict"] == "pass" && ca_v["dual_consensus"] == true);
    assert_eq!(w.events("t1", &[EventKind::DualReviewTurn]).len(), 2);

    let (d, r) = (&w.deps.settings.data_dir, &w.deps.settings.repo);
    let ho = load_handover(&w.db, d, "t1", r).unwrap();
    assert!(ho.status == "XONG" && ho.missing.is_empty() && ho.dual_consensus);
    assert_eq!(ho.reviewers.len(), 2);
}

#[test]
fn dual_review_branch4_adjudication_rebut_p1_vs_conservative_p0() {
    // Subcase A: P1 rebutted with valid file:line citation -> dropped -> consensus Review.
    let w = World::new(false, "t-p1", TaskState::Ready);
    w.imp.push("implemented");
    let p1 = issue("P1-1", "P1", "correctness", "src/lib.rs:1");
    w.rev_a.push(dual(&[], p1, 96, "nit"));
    w.rev_b.push(pass_dual(98, "clean"));
    w.rev_b.push(adj("P1-1", "REBUT", "src/lib.rs:1", "ok"));
    w.assert_review("t-p1", &w.run_once(None), true);
    assert_eq!((w.rev_a.reqs().len(), w.rev_b.reqs().len()), (1, 2));

    // Subcase B: P0 conservative rule -> REBUT with valid citation STILL keeps P0 -> rework.
    let w2 = World::new(false, "t-p0", TaskState::Ready);
    w2.imp.push("implemented");
    let iss = issue("SEC-1", "P0", "SECRET", "src/lib.rs:1");
    w2.rev_a
        .push(dual(&[("SECRET", "src/lib.rs:1")], iss, 96, "s"));
    w2.rev_b.push(pass_dual(98, "clean"));
    w2.rev_b.push(adj("SEC-1", "REBUT", "src/lib.rs:1", "no"));
    let sum_b = w2.run_once(None);
    assert_eq!(w2.rev_b.reqs().len(), 2);
    w2.assert_rework("t-p0", &sum_b, "src/lib.rs:1");
}

#[test]
fn dual_review_sibling_failure_preserves_partial_and_recovers_with_zero_missing() {
    // Subcase A: Sibling A hits 429 LimitHit while Sibling B finishes PASS -> B cached on disk.
    let w = World::new(false, "t-part", TaskState::Gating);
    let sync = SyncGate::default();
    w.rev_a.arm(sync.clone());
    w.rev_b.arm(sync);
    w.rev_a.push_limit(Some(10_000), "429 RESOURCE_EXHAUSTED");
    w.rev_b.push(pass_dual(98, "clean"));

    let s1 = w.run_once(None);
    assert!(s1.review.is_empty() && s1.blocked.is_empty() && s1.errors.is_empty());
    assert_eq!(w.db.get_task("t-part").unwrap().state, TaskState::Gating);
    assert!(w.events("t-part", &[EventKind::CrossAudit]).is_empty());
    assert_eq!((w.rev_a.reqs().len(), w.rev_b.reqs().len()), (1, 1));

    let (d, wt) = (&w.deps.settings.data_dir, w.wt("t-part"));
    let key = aaf_fp::load_key(d).unwrap();
    let bind = PartialAuditKey {
        repo_root: &wt,
        tree_fp: TREE_FP,
        base_sha: BASE_SHA,
        config_hash: &w.cfg_hash,
        plan_contract_hash: "",
        reviewer_id: REV_B.0,
        executor: REV_B.1,
        model: REV_B.2,
        effort: "high",
    };
    let got = aaf_fp::read_partial_audit(d, &key, &bind).unwrap().unwrap();
    assert_eq!(got.verdict, "PASS");

    // Next run: ONLY Reviewer A runs (B cache hits), consensus signs.
    w.rev_a.push(pass_dual(96, "recovered"));
    w.assert_review("t-part", &w.run_once(None), false);
    assert_eq!((w.rev_a.reqs().len(), w.rev_b.reqs().len()), (2, 1));

    // Subcase B: 0-missing-partials SIGKILL crash recovery (R3-CONC-1).
    let w2 = World::new(false, "t-crash", TaskState::Gating);
    let (d2, wt2) = (&w2.deps.settings.data_dir, w2.wt("t-crash"));
    std::fs::create_dir_all(&wt2).unwrap();
    let canon = std::fs::canonicalize(&wt2).unwrap();
    let key2 = aaf_fp::load_or_create_key(d2).unwrap();
    for (rev, cp, sc) in [(REV_A, "gemini", 96u8), (REV_B, "claude", 98u8)] {
        let rec: PartialAuditRecord = from_j(json!({
            "repo_root": canon.to_string_lossy(), "tree_fp": TREE_FP, "base_sha": BASE_SHA,
            "config_hash": w2.cfg_hash, "plan_contract_hash": "", "reviewer_id": rev.0,
            "executor": rev.1, "canonical_provider": cp, "model": rev.2, "effort": "high",
            "verdict": "PASS", "p0": 0, "p1": 0, "p2": 0, "mechanical_score": 100,
            "llm_average": sc, "effective_score": sc, "scores": scores(sc, "cached"),
            "issues": [], "created_at": 1_000
        }));
        aaf_fp::write_partial_audit(d2, &key2, &wt2, &rec).unwrap();
    }
    w2.assert_review("t-crash", &w2.run_once(None), false);
    assert!(w2.rev_a.reqs().is_empty() && w2.rev_b.reqs().is_empty());
}

#[test]
fn dual_review_pre_gate_audit_does_not_bypass_second_reviewer_and_budget_race_safe() {
    // Part A: Head-locked task runs pre-gate Job::Audit -> post-gate Dual-Review STILL runs.
    let w = World::new(true, "t-lock", TaskState::Ready);
    let pre = Arc::<QueueRunner>::default();
    pre.push(json!({"answers": answers(&[]), "scores": scores(97, "ok")}).to_string());
    let audit = AuditBackend {
        provider: "antigravity".into(),
        runner: pre.clone(),
        accounts: Arc::new(StubPool(vec![])),
        hook_required: false,
    };
    w.imp.push("edited locked ci workflow");
    w.rev_a.push(pass_dual(96, "rev-a ok"));
    w.rev_b.push(pass_dual(98, "rev-b ok"));
    w.assert_review("t-lock", &w.run_once(Some(&audit)), false);
    assert_eq!((pre.reqs().len(), w.rev_b.reqs().len()), (1, 1));

    // Part B: Exhausted RootBudget entering NeedsPostAudit checks ONCE on scheduler (R3-CONC-4).
    let w2 = World::new(false, "t-budget", TaskState::Gating);
    let mut b = RootBudget::new("t-budget", &w2.deps.settings.repo.unblocker);
    b.tokens_used = b.token_limit + 1;
    w2.db.put_budget(&b).unwrap();
    let sum_b = w2.run_once(None);
    assert_eq!(sum_b.blocked, vec!["t-budget"]);
    assert!(sum_b.errors.is_empty() && w2.rev_a.reqs().is_empty() && w2.rev_b.reqs().is_empty());
    let tb = w2.db.get_task("t-budget").unwrap();
    assert_eq!(tb.state, TaskState::Blocked);
    let is_budget = matches!(
        tb.blocked_reason,
        Some(BlockedReason::BudgetExceeded { .. })
    );
    assert!(is_budget);
    let fails = w2.db.get_meta(META_CONSECUTIVE_ROOT_FAILS).unwrap();
    assert_eq!(fails.as_deref(), Some("1"));
}

#[test]
fn dual_review_plan_drift_omission_flows_to_implementer_rework() {
    let w = World::new(false, "t-drift", TaskState::Ready);
    let pc: PlanContract = from_j(json!({
        "summary": "Add new module", "tier_assessment": "M", "tier_reasons": ["feature"],
        "files_to_change": [{"path": "src/new_module.rs", "responsibility": "core", "risk": "low"}],
        "invariants_and_edge_cases": ["reject empty"], "dispositions": [], "open_questions": [],
        "red_green_tests": [{"ac_ref": "AC-1", "test_path": "tests/new_module_test.rs",
                             "red_condition": "fails", "green_condition": "passes"}]
    }));
    let body = json!({"summary": pc.summary, "plan_contract_hash": canonical_plan_hash(&pc), "plan_contract": pc});
    let ev = FactoryEvent::new(EventKind::PlanConverged, body.to_string(), 900).for_task("t-drift");
    w.db.emit(&ev).unwrap();
    w.imp.push("implemented without new_module");
    let iss = issue("DRIFT-1", "P1", "PLAN-DRIFT", "src/new_module.rs");
    let reply = dual(&[("PLAN-DRIFT", "src/new_module.rs")], iss, 96, "missing");
    w.rev_a.push(&reply);
    w.rev_b.push(&reply);
    w.assert_rework("t-drift", &w.run_once(None), "src/new_module.rs:1");
    assert!(w.events("t-drift", &[EventKind::DetectorHit]).is_empty());
}

#[test]
fn dual_review_adjudicate_repeated_oauth_refreshed_halts_instead_of_looping() {
    let w = World::new(false, "t-adj-loop", TaskState::Gating);
    // Sibling A finds a P1 bug (with passing scores >= 95); Sibling B passes -> triggers AdjudicateTurn on rev_b.
    let iss = issue("ADJ-1", "P1", "correctness", "src/lib.rs:1");
    w.rev_a.push(dual(&[], iss, 96, "bug"));
    w.rev_b.push(pass_dual(97, "looks clean"));
    // AdjudicateTurn hits OAuthRefreshed twice in a row (attempt 1 retries once,
    // attempt 2 MUST halt rather than having `missing.is_empty()` wipe the counter).
    w.rev_b.push_oauth_refreshed("oauth refreshed #1");
    w.rev_b.push_oauth_refreshed("oauth refreshed #2");

    let sum = w.run_once(None);
    assert!(
        sum.halted.is_some(),
        "repeated AdjudicateTurn OAuthRefreshed must halt after 1 retry, got: {sum:?}"
    );
    assert_eq!(
        w.rev_a.reqs().len(),
        1,
        "cached rev_a partial must not re-run"
    );
    assert_eq!(
        w.rev_b.reqs().len(),
        3,
        "rev_b should run 1 blind turn + 2 adjudicate attempts (initial + 1 retry)"
    );
}
