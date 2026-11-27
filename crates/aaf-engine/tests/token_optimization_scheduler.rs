#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Scheduler integration test verifying Plan Debate stage `max_turns` limits,
//! Round 2+ Delta-Only prompt switch, and format-repair prior-draft injection.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::config::RepoConfig;
use aaf_domain::gate::{
    GateMode, GateStatus, HeadLockReport, LockStatus, RECEIPT_SCHEMA_VERSION, Receipt,
    StoreComplianceReport,
};
use aaf_domain::pipeline::{
    DispositionAction, PlanContract, PlanDisposition, PlanFileChange, RedGreenTestPlan,
};
use aaf_domain::ports::{
    AccountStore, AgentOutcome, AgentRequest, AgentRunner, Clock, LimitHit, PortResult,
    TaskStore as _,
};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::factory::adapters::WebhookNotifier;
use aaf_engine::factory::{
    AccountPool, Deps, GateRunner, GitOps, Grant, Hooks, Integration, Mode, Settings, StageRouter,
    StageTarget, run_with_router,
};
use aaf_engine::git::{MergeOutcome, PrOutcome, TaskWorktree};
use aaf_engine::plan_debate::prompt::PRIOR_BEGIN;
use aaf_engine::plan_debate::schema::{plan_contract_json_schema, plan_critique_json_schema};
use aaf_engine::unblocker::QuotaRoute;

type RegateFn<'a> = &'a dyn Fn(&Path) -> Result<bool, String>;

#[derive(Default)]
struct QueueRunner(Mutex<(VecDeque<String>, Vec<AgentRequest>)>);

impl QueueRunner {
    fn with_replies(replies: Vec<String>) -> Arc<Self> {
        Arc::new(Self(Mutex::new((replies.into(), Vec::new()))))
    }
    fn reqs(&self) -> Vec<AgentRequest> {
        self.0.lock().unwrap().1.clone()
    }
}

impl AgentRunner for QueueRunner {
    fn run(&self, request: &AgentRequest) -> PortResult<AgentOutcome> {
        let mut g = self.0.lock().unwrap();
        g.1.push(request.clone());
        let final_text = g.0.pop_front().unwrap_or_default();
        Ok(AgentOutcome {
            exit_code: Some(0),
            timed_out: false,
            final_text,
            tokens_in: 100,
            tokens_out: 50,
            limit_hit: None,
            infra: None,
            log_tail: String::new(),
        })
    }
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

struct StubInfra(PathBuf, String, String);
impl Clock for StubInfra {
    fn now(&self) -> i64 {
        1_700_000_000
    }
}
impl GitOps for StubInfra {
    fn task_worktree(&self, id: &str) -> PortResult<TaskWorktree> {
        Ok(TaskWorktree {
            path: self.0.clone(),
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
        Ok(self.2.clone())
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

fn make_critique_json(
    verdict: &str,
    ac2_pass: bool,
    issues: serde_json::Value,
    resolved: serde_json::Value,
) -> String {
    let chk: Vec<_> = [
        ("PLAN-AC-1", true),
        ("PLAN-AC-2", ac2_pass),
        ("PLAN-SCOPE", true),
        ("PLAN-SEC", true),
        ("PLAN-TEST", ac2_pass),
        ("PLAN-ROLLBACK", true),
    ]
    .into_iter()
    .map(|(id, pass)| {
        serde_json::json!({"id": id, "pass": pass, "evidence": if pass { "ok" } else { "missing AC-2" }})
    })
    .collect();
    serde_json::json!({
        "verdict": verdict,
        "checklist": chk,
        "issues": issues,
        "resolved_from_previous": resolved
    })
    .to_string()
}

#[test]
fn plan_debate_scheduler_enforces_stage_max_turns_and_repair_prior() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    std::fs::create_dir_all(repo_dir.join("src")).unwrap();
    std::fs::write(repo_dir.join("src/parser.rs"), "pub fn parse() {}\n").unwrap();
    std::fs::create_dir_all(repo_dir.join("tests")).unwrap();
    std::fs::write(repo_dir.join("tests/parser.rs"), "#[test] fn t() {}\n").unwrap();
    let repo_root = repo_dir.to_string_lossy().into_owned();

    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    let mut task = Task::new_root(
        "t-tok-1",
        &repo_root,
        "Optimize parser",
        ImmutableSpec {
            description: "Handle empty input cleanly.".into(),
            acceptance: vec![
                "Empty input returns Err".into(),
                "Existing valid input passes unchanged".into(),
            ],
        },
        Priority::P1,
        1,
    );
    task.state = TaskState::Ready;
    task.scope_paths = vec!["src/parser.rs".into()];
    db.insert_task(&task).unwrap();

    let acct = AccountProfile {
        id: "claude-1".into(),
        provider: ProviderKind::ClaudeCode,
        config_dir: tmp.path().join("claude-1").to_string_lossy().into_owned(),
        name: "claude-1".into(),
    };
    db.upsert_account(&acct).unwrap();
    let cp = Arc::new(StubPool(vec![acct]));

    let plan_r1 = PlanContract {
        summary: "Guard empty input in parser.".into(),
        tier_assessment: "M".into(),
        tier_reasons: vec!["touches parser and tests".into()],
        files_to_change: vec![PlanFileChange {
            path: "src/parser.rs".into(),
            responsibility: "Reject empty input".into(),
            risk: "low".into(),
        }],
        red_green_tests: vec![RedGreenTestPlan {
            ac_ref: "AC-1".into(),
            test_path: "tests/parser.rs".into(),
            red_condition: "empty input panics".into(),
            green_condition: "returns Err".into(),
        }],
        invariants_and_edge_cases: vec!["whitespace-only input".into()],
        dispositions: vec![],
        open_questions: vec![],
    };
    let valid_plan_r1 = serde_json::to_string(&plan_r1).unwrap();
    let mut plan_r2 = plan_r1;
    plan_r2.red_green_tests.push(RedGreenTestPlan {
        ac_ref: "AC-2".into(),
        test_path: "tests/parser.rs".into(),
        red_condition: "valid input rejected".into(),
        green_condition: "valid input succeeds".into(),
    });
    plan_r2.dispositions.push(PlanDisposition {
        issue_id: "ISS-1".into(),
        action: DispositionAction::Accepted,
        response: "Added AC-2 test in tests/parser.rs".into(),
        evidence: "tests/parser.rs".into(),
    });
    let valid_plan_r2 = serde_json::to_string(&plan_r2).unwrap();

    let critique_r1 = make_critique_json(
        "REVISE",
        false,
        serde_json::json!([{
            "id": "ISS-1", "severity": "P1", "category": "test",
            "location": "tests/parser.rs", "description": "Missing AC-2 test",
            "proposed_fix": "Add AC-2 test"
        }]),
        serde_json::json!([]),
    );
    let critique_r2 = make_critique_json(
        "CONVERGED",
        true,
        serde_json::json!([]),
        serde_json::json!([{
            "issue_id": "ISS-1", "status": "VERIFIED_FIXED", "reason": "AC-2 test added"
        }]),
    );

    let author = QueueRunner::with_replies(vec![
        "{\"summary\":\"partial broken json".into(),
        valid_plan_r1,
        valid_plan_r2,
    ]);
    let chal = QueueRunner::with_replies(vec![critique_r1, critique_r2]);
    let imp = QueueRunner::with_replies(vec![]);

    let mut repo = RepoConfig::default();
    repo.run.executor = "claude".into();
    repo.pipeline.plan_debate = true;
    repo.pipeline.max_plan_rounds = 3;
    repo.pipeline.require_plan_approval = true;
    repo.pipeline.allow_tier_s_skip_debate = false;

    let infra = Arc::new(StubInfra(
        repo_dir,
        aaf_engine::gate::config_hash(&repo),
        String::new(),
    ));
    let mk_target = |exec: &str, r: Arc<dyn AgentRunner>, pool: Arc<dyn AccountPool>| {
        let mut t = StageTarget::new(exec, "", "", "", r, pool);
        t.hook_required = false;
        t
    };
    let router = StageRouter::new(mk_target("claude", imp.clone(), cp.clone()))
        .with_plan_author(mk_target("claude", author.clone(), cp.clone()))
        .with_plan_challenger(mk_target("codex", chal.clone(), Arc::new(StubPool(vec![]))));

    let deps = Deps {
        store: Arc::new(db),
        clock: infra.clone(),
        runner: imp,
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
            data_dir: tmp.path().to_path_buf(),
            home: tmp.path().to_path_buf(),
            owner_github_allowlist: Vec::new(),
        },
    };
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };
    run_with_router(&deps, None, Some(&router), Mode::Once, &hooks).unwrap();

    let author_reqs = author.reqs();
    assert_eq!(
        author_reqs.iter().map(|r| r.max_turns).collect::<Vec<_>>(),
        vec![Some(22), Some(8), Some(12)]
    );
    assert!(author_reqs[0].prompt.contains(plan_contract_json_schema()));
    assert!(author_reqs[1].prompt.contains(PRIOR_BEGIN));
    assert!(author_reqs[1].prompt.contains("partial broken json"));
    assert!(author_reqs[2].prompt.contains("DELTA-ONLY REVISION"));

    let chal_reqs = chal.reqs();
    assert_eq!(
        chal_reqs.iter().map(|r| r.max_turns).collect::<Vec<_>>(),
        vec![Some(16), Some(10)]
    );
    assert!(chal_reqs[0].prompt.contains(plan_critique_json_schema()));
    assert!(chal_reqs[1].prompt.contains("DELTA RE-VERIFICATION"));
}

#[test]
fn dual_review_scheduler_enforces_max_turns_and_preserves_tail_scores_on_repair() {
    use aaf_domain::pipeline::ReviewerConfig;
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    std::fs::create_dir_all(repo_dir.join("src")).unwrap();
    std::fs::write(repo_dir.join("src/parser.rs"), "pub fn parse() {}\n").unwrap();
    let repo_root = repo_dir.to_string_lossy().into_owned();

    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    let mut task = Task::new_root(
        "t-dual-1",
        &repo_root,
        "Fix parser",
        ImmutableSpec {
            description: "Fix parser.".into(),
            acceptance: vec!["handles empty".into()],
        },
        Priority::P1,
        1,
    );
    task.state = TaskState::Ready;
    db.insert_task(&task).unwrap();

    let acct = AccountProfile {
        id: "claude-1".into(),
        provider: ProviderKind::ClaudeCode,
        config_dir: tmp.path().join("claude-1").to_string_lossy().into_owned(),
        name: "claude-1".into(),
    };
    db.upsert_account(&acct).unwrap();
    let cp = Arc::new(StubPool(vec![acct]));

    let ok_scores = serde_json::json!([
        {"id": "G1", "score": 95, "deduction_reason": "minor"},
        {"id": "G2", "score": 95, "deduction_reason": "minor"},
        {"id": "G3", "score": 95, "deduction_reason": "minor"},
        {"id": "G4", "score": 95, "deduction_reason": "minor"},
        {"id": "G5", "score": 95, "deduction_reason": "minor"},
        {"id": "G6", "score": 95, "deduction_reason": "minor"},
        {"id": "G7", "score": 95, "deduction_reason": "minor"}
    ]);
    let answers: Vec<_> =
        aaf_engine::cross_audit::dual_verdict::build_dual_checklist(&task.spec, false, false)
            .into_iter()
            .map(|it| serde_json::json!({"id": it.id, "answer": "no", "evidence": ""}))
            .collect();
    let valid_pass = serde_json::json!({
        "answers": answers,
        "issues": [],
        "scores": ok_scores
    })
    .to_string();
    // > 14,000-char malformed reply exceeds DUAL_REPAIR_PRIOR_CHARS (12,000) so
    // middle-clipping actually triggers and preserves the tail `scores`/marker.
    let bloated_malformed = format!(
        "{{\"answers\":[],\"padding\":\"{}\",\"tail_marker\":\"G7_TAIL_PRESERVED\"}}",
        "a".repeat(14_000)
    );

    let rev_a = QueueRunner::with_replies(vec![bloated_malformed, valid_pass.clone()]);
    let rev_b = QueueRunner::with_replies(vec![valid_pass]);
    let imp = QueueRunner::with_replies(vec!["implemented".into()]);

    let mut repo = RepoConfig::default();
    repo.run.executor = "claude".into();
    repo.pipeline.plan_debate = false;
    repo.cross_audit.dual_review = true;
    repo.cross_audit.score_gate = true;
    repo.cross_audit.reviewers = vec![
        ReviewerConfig {
            id: "rev-claude".into(),
            executor: "claude".into(),
            model: String::new(),
            effort: String::new(),
            executor_cmd: String::new(),
        },
        ReviewerConfig {
            id: "rev-codex".into(),
            executor: "codex".into(),
            model: String::new(),
            effort: String::new(),
            executor_cmd: String::new(),
        },
    ];

    let infra = Arc::new(StubInfra(
        repo_dir,
        aaf_engine::gate::config_hash(&repo),
        "diff --git a/src/parser.rs b/src/parser.rs\n--- a/src/parser.rs\n+++ b/src/parser.rs\n@@ -1 +1 @@\n-pub fn parse() {}\n+pub fn parse(s: &str) -> bool { !s.is_empty() }\n".into(),
    ));
    let mk_target = |exec: &str, r: Arc<dyn AgentRunner>, pool: Arc<dyn AccountPool>| {
        let mut t = StageTarget::new(exec, "", "", "", r, pool);
        t.hook_required = false;
        t
    };
    let router =
        StageRouter::new(mk_target("claude", imp.clone(), cp.clone())).with_reviewers(vec![
            (
                "rev-claude".into(),
                mk_target("claude", rev_a.clone(), cp.clone()),
            ),
            (
                "rev-codex".into(),
                mk_target("codex", rev_b.clone(), Arc::new(StubPool(vec![]))),
            ),
        ]);

    let deps = Deps {
        store: Arc::new(db),
        clock: infra.clone(),
        runner: imp,
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
            data_dir: tmp.path().to_path_buf(),
            home: tmp.path().to_path_buf(),
            owner_github_allowlist: Vec::new(),
        },
    };
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };
    run_with_router(&deps, None, Some(&router), Mode::Once, &hooks).unwrap();

    let a_reqs = rev_a.reqs();
    assert_eq!(
        a_reqs.iter().map(|r| r.max_turns).collect::<Vec<_>>(),
        vec![Some(16), Some(8)]
    );
    assert!(a_reqs[1].prompt.contains("...[middle omitted]..."));
    assert!(a_reqs[1].prompt.contains("G7_TAIL_PRESERVED"));
    let b_reqs = rev_b.reqs();
    assert_eq!(
        b_reqs.iter().map(|r| r.max_turns).collect::<Vec<_>>(),
        vec![Some(16)]
    );
}

#[test]
fn proposer_enforces_initial_and_repair_max_turns() {
    use aaf_domain::blocked::BlockedReason;
    use aaf_domain::budget::RootBudget;
    use aaf_domain::ports::{BudgetStore, SandboxPolicy};
    use aaf_engine::unblocker::{AgentSubtaskProposer, SubtaskProposer};

    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    let mut task = Task::new_root(
        "t-prop-1",
        "/repo",
        "Big task",
        ImmutableSpec {
            description: "Split me.".into(),
            acceptance: vec!["part a".into(), "part b".into()],
        },
        Priority::P1,
        1,
    );
    task.state = TaskState::Ready;
    db.insert_task(&task).unwrap();
    db.put_budget(&RootBudget::new(
        "t-prop-1",
        &aaf_domain::config::UnblockerConfig::default(),
    ))
    .unwrap();

    let runner = QueueRunner::with_replies(vec!["not valid json".into(), "[]".into()]);
    let proposer = AgentSubtaskProposer {
        runner: &*runner,
        budgets: &db,
        base: AgentRequest {
            run_id: "prop-1".into(),
            prompt: String::new(),
            cwd: tmp.path().to_string_lossy().into_owned(),
            account: None,
            timeout_secs: 60,
            sandbox: SandboxPolicy {
                read_only: true,
                ..SandboxPolicy::default()
            },
            json_schema: None,
            max_turns: None,
        },
    };
    let plans = proposer
        .propose(
            &task,
            &BlockedReason::Stuck {
                detail: "too big".into(),
            },
            "d1",
        )
        .unwrap();
    assert!(plans.is_empty());
    assert_eq!(
        runner
            .reqs()
            .iter()
            .map(|r| r.max_turns)
            .collect::<Vec<_>>(),
        vec![Some(12), Some(6)]
    );
}
