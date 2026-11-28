#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Scheduler integration tests for Package 1 (#1-#5):
//! - Compact JSON format-repair turn preserving the anti-downgrade latch
//! - Stopping deterministic `INCONCLUSIVE` citation/downgrade retries without `INFRA_RETRY_MAX` loops
//! - Deterministic `SCOPE-HARD` overriding `ConsensusPass`

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use aaf_domain::account::AccountProfile;
use aaf_domain::config::RepoConfig;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::gate::*;
use aaf_domain::pipeline::{PlanContract, PlanFileChange, RedGreenTestPlan};
use aaf_domain::ports::*;
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::cross_audit::GOLDEN_IDS;
use aaf_engine::cross_audit::dual_verdict::build_dual_checklist;
use aaf_engine::cross_audit::prompt::DIFF_BEGIN;
use aaf_engine::db::Db;
use aaf_engine::factory::adapters::WebhookNotifier;
use aaf_engine::factory::*;
use aaf_engine::gate::config_hash;
use aaf_engine::git::{MergeOutcome, PrOutcome, TaskWorktree as Wt};
use aaf_engine::plan_debate::canonical_plan_hash;
use aaf_engine::unblocker::QuotaRoute;
use serde_json::{Value, json};
use tempfile::TempDir;

type RegateFn<'a> = &'a dyn Fn(&Path) -> Result<bool, String>;

const REV_A: (&str, &str, &str) = ("antigravity-neon", "antigravity", "neon-test");
const REV_B: (&str, &str, &str) = ("claude-opus", "claude", "opus-test");
const TREE_FP: &str = "git-tree:pkg1";
const BASE_SHA: &str = "base-pkg1";

fn sample_task(id: &str, scope_paths: Vec<&str>) -> Task {
    let mut t = Task::new_root(
        id,
        "/repo",
        "Package 1 task",
        ImmutableSpec {
            description: "Implement planned feature".into(),
            acceptance: vec!["planned feature works".into()],
        },
        Priority::P1,
        100,
    );
    t.scope_paths = scope_paths.into_iter().map(str::to_string).collect();
    t
}

fn sample_plan() -> PlanContract {
    PlanContract {
        summary: "Update parser".into(),
        tier_assessment: "M".into(),
        tier_reasons: vec!["scoped".into()],
        files_to_change: vec![PlanFileChange {
            path: "src/feature/parser.rs".into(),
            responsibility: "parse input".into(),
            risk: "low".into(),
        }],
        invariants_and_edge_cases: vec!["no panic".into()],
        red_green_tests: vec![RedGreenTestPlan {
            ac_ref: "AC-1".into(),
            test_path: "tests/feature_test.rs".into(),
            red_condition: "fails".into(),
            green_condition: "passes".into(),
        }],
        dispositions: vec![],
        open_questions: vec![],
    }
}

fn scores_all(score: u8) -> Value {
    let reason = if score < 100 { "minor" } else { "" };
    GOLDEN_IDS
        .iter()
        .map(|id| json!({"id": id, "score": score, "deduction_reason": reason}))
        .collect()
}

fn dual_answers(
    checklist: &[aaf_engine::cross_audit::ChecklistItem],
    yes: &[(&str, &str)],
) -> Value {
    checklist
        .iter()
        .map(|it| {
            let ev = yes.iter().find(|(k, _)| *k == it.id).map(|(_, e)| *e);
            json!({
                "id": it.id,
                "answer": if ev.is_some() { "yes" } else { "no" },
                "evidence": ev.unwrap_or(""),
            })
        })
        .collect()
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
struct QueueRunner(Mutex<(VecDeque<AgentOutcome>, Vec<AgentRequest>)>);
impl QueueRunner {
    fn push(&self, text: impl AsRef<str>) {
        self.0.lock().unwrap().0.push_back(AgentOutcome {
            exit_code: Some(0),
            timed_out: false,
            final_text: text.as_ref().to_string(),
            tokens_in: 50,
            tokens_out: 25,
            limit_hit: None,
            infra: None,
            log_tail: String::new(),
        });
    }
    fn reqs(&self) -> Vec<AgentRequest> {
        self.0.lock().unwrap().1.clone()
    }
}
impl AgentRunner for QueueRunner {
    fn run(&self, req: &AgentRequest) -> PortResult<AgentOutcome> {
        let mut g = self.0.lock().unwrap();
        g.1.push(req.clone());
        Ok(g.0.pop_front().unwrap_or(AgentOutcome {
            exit_code: Some(0),
            timed_out: false,
            final_text: String::new(),
            tokens_in: 0,
            tokens_out: 0,
            limit_hit: None,
            infra: None,
            log_tail: String::new(),
        }))
    }
}

struct StubInfra {
    root: PathBuf,
    cfg_hash: String,
    diff: String,
}
impl Clock for StubInfra {
    fn now(&self) -> i64 {
        2_000
    }
}
impl GitOps for StubInfra {
    fn task_worktree(&self, id: &str) -> PortResult<Wt> {
        let path = self.root.join(format!("wt-{id}"));
        fs::create_dir_all(&path).unwrap();
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
        Ok(self.diff.clone())
    }
    fn integrate(&self, ids: &[String]) -> PortResult<Integration> {
        Ok(Integration {
            tree: self.task_worktree("int")?,
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
            new_base_sha: "base2".into(),
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
        Ok(serde_json::from_value(json!({
            "schema_version": RECEIPT_SCHEMA_VERSION, "repo_root": tree.to_string_lossy(),
            "tree_fp": TREE_FP, "head_sha": "h1", "base_sha": BASE_SHA, "mode": "scoped",
            "status": "CLEAR", "reasons": [], "config_hash": self.cfg_hash, "commands": [],
            "static_findings": [], "suppressions": 0, "created_at": 2_000,
            "head_lock": {"status": "CLEAR", "lock_fp": "", "locked_changes": [], "approval": null},
            "store_compliance": StoreComplianceReport::not_applicable()
        }))
        .unwrap())
    }
    fn head_lock(&self, _: &Path) -> PortResult<HeadLockReport> {
        Ok(serde_json::from_value(json!({
            "status": "CLEAR", "lock_fp": "", "locked_changes": [], "approval": null
        }))
        .unwrap())
    }
    fn tree_binding(&self, _: &Path) -> Option<(String, Option<String>)> {
        Some((TREE_FP.into(), Some(BASE_SHA.into())))
    }
}

struct World {
    _tmp: TempDir,
    db: Db,
    deps: Deps,
    router: StageRouter,
    rev_a: Arc<QueueRunner>,
    rev_b: Arc<QueueRunner>,
}

impl World {
    fn new(id: &str, diff: &str, scope_paths: Vec<&str>, plan: Option<PlanContract>) -> Self {
        let tmp = TempDir::new().unwrap();
        let repo_dir = tmp.path().join("repo");
        fs::create_dir_all(&repo_dir).unwrap();
        let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
        let mut t = sample_task(id, scope_paths);
        t.repo_root = repo_dir.to_string_lossy().into_owned();
        t.state = TaskState::Gating;
        db.insert_task(&t).unwrap();
        if let Some(pc) = plan {
            let body = json!({
                "summary": pc.summary,
                "plan_contract_hash": canonical_plan_hash(&pc),
                "plan_contract": pc,
            });
            db.emit(
                &FactoryEvent::new(EventKind::PlanConverged, body.to_string(), 900).for_task(id),
            )
            .unwrap();
        }
        let c1 = tmp.path().join("c1").to_string_lossy().into_owned();
        let acct: AccountProfile = serde_json::from_value(json!({
            "id": "claude-1", "provider": "claude_code", "config_dir": c1, "name": "claude-1"
        }))
        .unwrap();
        db.upsert_account(&acct).unwrap();
        let (cp, ap) = (Arc::new(StubPool(vec![acct])), Arc::new(StubPool(vec![])));
        let repo: RepoConfig = serde_json::from_value(json!({
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
        }))
        .unwrap();
        let cfg_hash = config_hash(&repo);
        let dir = tmp.path().to_path_buf();
        let infra = Arc::new(StubInfra {
            root: dir.clone(),
            cfg_hash,
            diff: diff.to_string(),
        });
        let (imp, rev_a, rev_b): (Arc<QueueRunner>, Arc<QueueRunner>, Arc<QueueRunner>) =
            Default::default();
        let mk = |e: &str, m: &str, r: Arc<dyn AgentRunner>, p: Arc<dyn AccountPool>| {
            let mut st = StageTarget::new(e, "", m, "high", r, p);
            st.hook_required = false;
            st
        };
        let router =
            StageRouter::new(mk("claude", "", imp.clone(), cp.clone())).with_reviewers(vec![
                (REV_A.0.into(), mk(REV_A.1, REV_A.2, rev_a.clone(), ap)),
                (
                    REV_B.0.into(),
                    mk(REV_B.1, REV_B.2, rev_b.clone(), cp.clone()),
                ),
            ]);
        let settings = Settings {
            repo_root: repo_dir.to_string_lossy().into_owned(),
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
            runner: imp,
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
            rev_a,
            rev_b,
        }
    }

    fn run_once(&self) -> RunSummary {
        let hooks = Hooks {
            idle: &|| false,
            reap: &|_| Ok(()),
        };
        run_with_router(&self.deps, None, Some(&self.router), Mode::Once, &hooks).unwrap()
    }
}

#[test]
fn compact_repair_omits_diff_preserves_anti_downgrade_and_stops_infra_retry_loop() {
    let diff = "diff --git a/src/lib.rs b/src/lib.rs\n--- /dev/null\n+++ b/src/lib.rs\n@@ -0,0 +1,2 @@\n+pub fn a() {}\n+pub fn b() {}\n";
    let checklist = build_dual_checklist(&sample_task("t-rep", vec![]).spec, false, false);
    let clean_pass = json!({
        "answers": dual_answers(&checklist, &[]),
        "issues": [],
        "scores": scores_all(98)
    })
    .to_string();

    // Case 1A: Completed review (all checklist IDs + all 7 scores) with extra unknown key
    // uses compact format-only repair (no DIFF, max_turns=3) and reaches Review cleanly.
    let w_compact = World::new("t-compact", diff, vec![], None);
    let extra_field_pass = json!({
        "answers": dual_answers(&checklist, &[]),
        "issues": [],
        "scores": scores_all(98),
        "notes": "extra wrapper commentary"
    })
    .to_string();
    w_compact.rev_a.push(&extra_field_pass);
    w_compact.rev_a.push(&clean_pass);
    w_compact.rev_b.push(&clean_pass);
    let sum_c = w_compact.run_once();
    assert_eq!(sum_c.review, vec!["t-compact"]);
    let c_reqs = w_compact.rev_a.reqs();
    assert_eq!(c_reqs.len(), 2, "expected initial + 1 compact repair turn");
    assert_eq!(c_reqs[1].max_turns, Some(3));
    assert!(
        !c_reqs[1].prompt.contains(DIFF_BEGIN),
        "compact JSON syntax repair must omit full DIFF block when all items+scores were already answered"
    );

    // Case 1A-2: Extra-field JSON containing a SECRET=yes blocker MUST latch anti-downgrade
    // even when strict extract_json fails (prevents compact-repair downgrade bypass).
    let w_extra_blocker = World::new("t-extra-blk", diff, vec![], None);
    let extra_field_blocker = json!({
        "answers": dual_answers(&checklist, &[("SECRET", "src/lib.rs:1")]),
        "issues": [],
        "scores": scores_all(96),
        "notes": "found secret"
    })
    .to_string();
    w_extra_blocker.rev_a.push(&extra_field_blocker);
    w_extra_blocker.rev_a.push(&clean_pass);
    w_extra_blocker.rev_b.push(&clean_pass);
    let sum_eb = w_extra_blocker.run_once();
    assert_eq!(sum_eb.blocked, vec!["t-extra-blk"]);
    assert_eq!(
        w_extra_blocker
            .db
            .get_task("t-extra-blk")
            .unwrap()
            .retry_after,
        None
    );

    // Case 1A-3: Incomplete/empty-answers JSON MUST NOT use compact repair (must keep DIFF).
    let w_incomplete = World::new("t-incomp", diff, vec![], None);
    w_incomplete
        .rev_a
        .push("{\"answers\":[],\"unexpected\":true}");
    w_incomplete.rev_a.push(&clean_pass);
    w_incomplete.rev_b.push(&clean_pass);
    let sum_inc = w_incomplete.run_once();
    assert_eq!(sum_inc.review, vec!["t-incomp"]);
    let inc_reqs = w_incomplete.rev_a.reqs();
    assert_eq!(inc_reqs[1].max_turns, Some(8));
    assert!(
        inc_reqs[1].prompt.contains(DIFF_BEGIN),
        "incomplete reply with empty answers must receive full DIFF + checklist questions"
    );

    // Case 1A-4: Truncated JSON containing a complete inner blocking answer MUST latch
    // anti-downgrade so attempt 1 cannot downgrade to PASS.
    let w_trunc = World::new("t-trunc-blk", diff, vec![], None);
    w_trunc.rev_a.push(
        r#"{"answers":[{"id":"SECRET","answer":"yes","evidence":"src/lib.rs:1"}],"issues":[{"severity":"P0""#,
    );
    w_trunc.rev_a.push(&clean_pass);
    w_trunc.rev_b.push(&clean_pass);
    let sum_tr = w_trunc.run_once();
    assert_eq!(sum_tr.blocked, vec!["t-trunc-blk"]);
    assert_eq!(
        w_trunc.db.get_task("t-trunc-blk").unwrap().retry_after,
        None
    );

    // Case 1A-5: Over-12k reply where clip_repair_prior excises the middle JSON object
    // MUST NOT use compact repair (must retain full DIFF).
    let w_big = World::new("t-big-clip", diff, vec![], None);
    let pad = "x".repeat(7_000);
    let buried_extra = format!("{pad}\n{extra_field_pass}\n{pad}");
    w_big.rev_a.push(&buried_extra);
    w_big.rev_a.push(&clean_pass);
    w_big.rev_b.push(&clean_pass);
    let sum_big = w_big.run_once();
    assert_eq!(sum_big.review, vec!["t-big-clip"]);
    let big_reqs = w_big.rev_a.reqs();
    assert_eq!(big_reqs[1].max_turns, Some(8));
    assert!(
        big_reqs[1].prompt.contains(DIFF_BEGIN),
        "clipped reply whose JSON was excised must receive full DIFF"
    );

    // Case 1B: Missing checklist ID keeps full DIFF + questions (never answers blind),
    // AND attempting to downgrade a prior named blocker still fires anti-downgrade latch
    // and blocks with retry_after=None (no 6x INFRA_RETRY_MAX loop).
    let w = World::new("t-rep", diff, vec![], None);
    let incomplete_with_blocker = json!({
        "answers": [{"id": "SECRET", "answer": "yes", "evidence": "src/lib.rs:1"}],
        "issues": [],
        "scores": scores_all(96)
    })
    .to_string();
    w.rev_a.push(&incomplete_with_blocker);
    w.rev_a.push(&clean_pass);
    w.rev_b.push(&clean_pass);

    let sum = w.run_once();
    assert_eq!(sum.blocked, vec!["t-rep"]);
    let a_reqs = w.rev_a.reqs();
    assert_eq!(a_reqs.len(), 2, "expected initial + 1 full repair turn");
    assert_eq!(a_reqs[1].max_turns, Some(8));
    assert!(
        a_reqs[1].prompt.contains(DIFF_BEGIN),
        "missing checklist id repair must keep full DIFF and checklist questions"
    );
    let task = w.db.get_task("t-rep").unwrap();
    assert_eq!(task.state, TaskState::Blocked);
    assert_eq!(
        task.retry_after, None,
        "deterministic downgrade inconclusive must NOT schedule INFRA_RETRY_MAX auto-retry"
    );

    let w2 = World::new("t-cite", diff, vec![], None);
    let bad_cite = json!({
        "answers": dual_answers(&checklist, &[]),
        "issues": [{
            "id": "BAD-1", "severity": "P1", "category": "LOGIC",
            "location": "src/nonexistent.rs:42", "description": "bad", "proposed_fix": "fix"
        }],
        "scores": scores_all(96)
    })
    .to_string();
    w2.rev_a.push(&bad_cite);
    w2.rev_a.push(&bad_cite);
    w2.rev_b.push(&clean_pass);

    let sum2 = w2.run_once();
    assert_eq!(sum2.blocked, vec!["t-cite"]);
    assert!(
        w2.rev_a.reqs()[1].prompt.contains(DIFF_BEGIN),
        "citation repair must retain DIFF block so reviewer can fix line numbers"
    );
    let task2 = w2.db.get_task("t-cite").unwrap();
    assert_eq!(task2.state, TaskState::Blocked);
    assert_eq!(task2.retry_after, None);
}

#[test]
fn hard_scope_violation_overrides_both_reviewers_passing_consensus() {
    let diff = "\
diff --git a/src/feature/parser.rs b/src/feature/parser.rs
--- /dev/null
+++ b/src/feature/parser.rs
@@ -0,0 +1,1 @@
+pub fn parse() {}
diff --git a/tests/feature_test.rs b/tests/feature_test.rs
--- /dev/null
+++ b/tests/feature_test.rs
@@ -0,0 +1,1 @@
+#[test] fn ok() {}
diff --git a/src/auth/bypass.rs b/src/auth/bypass.rs
--- /dev/null
+++ b/src/auth/bypass.rs
@@ -0,0 +1,1 @@
+pub fn bypass() {}
";
    let w = World::new(
        "t-scope",
        diff,
        vec!["src/feature", "tests"],
        Some(sample_plan()),
    );
    let checklist = build_dual_checklist(&sample_task("t-scope", vec![]).spec, false, true);
    let both_pass = json!({
        "answers": dual_answers(&checklist, &[]),
        "issues": [],
        "scores": scores_all(98)
    })
    .to_string();
    w.rev_a.push(&both_pass);
    w.rev_b.push(&both_pass);

    let sum = w.run_once();
    assert!(
        sum.review.is_empty(),
        "hard scope violation must block Review: {sum:?}"
    );
    let t = w.db.get_task("t-scope").unwrap();
    assert_eq!(
        t.state,
        TaskState::Ready,
        "hard scope violation must force rework"
    );
    assert!(
        t.context_notes
            .iter()
            .any(|n| n.contains("src/auth/bypass.rs:1") && n.contains("PLAN-SCOPE")),
        "rework notes should cite deterministic hard scope violation: {:?}",
        t.context_notes
    );
}
