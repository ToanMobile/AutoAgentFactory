#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Plan v3 Wave 2 Slice 4 tests (part 1 — see `plan_v3_audit_and_gates_extra.rs` for part 2):
//! - Deterministic allowlisted lockfile summarizer (`V2-SEC-13`)
//! - Forced post-gate audit on `behavioral_red_unproven` + checklist note (`V2-SEC-09`)
//! - Auditor retries on `Inconclusive` (max 2, >6000-char head+tail) & secondary auth fallback

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use aaf_domain::account::AccountProfile;
use aaf_domain::config::{AuditScope, RepoConfig};
use aaf_domain::gate::{
    GateMode, GateStatus, HeadLockReport, LockStatus, RECEIPT_SCHEMA_VERSION, Receipt,
    StoreComplianceReport,
};
use aaf_domain::ports::{
    AgentOutcome, AgentRequest, AgentRunner, Clock, InfraFailure, InfraKind, LimitHit, PortResult,
    TaskStore,
};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::cross_audit::prompt::{
    MAX_REPAIR_PRIOR_CHARS, build_format_repair_prompt, is_allowlisted_lockfile,
    summarize_diff_lockfiles,
};
use aaf_engine::cross_audit::{
    AuditVerdict, build_checklist, parse_diff_hunks, parse_scored_verdict,
    validate_verdict_against_diff,
};
use aaf_engine::db::Db;
use aaf_engine::factory::{
    AccountPool, AuditBackend, Deps, GateRunner, GitOps, Grant, Hooks, Integration, Mode, Notifier,
    Settings, run_with_audit,
};
use aaf_engine::git::{MergeOutcome, PrOutcome, TaskWorktree};
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

struct EmptyPool;
impl AccountPool for EmptyPool {
    fn acquire(&self) -> PortResult<Option<Grant>> {
        Ok(Some(Grant::new(None, None)))
    }
    fn after_limit(&self) -> PortResult<Option<QuotaRoute>> {
        Ok(Some(QuotaRoute::Wait))
    }
    fn report_limit(&self, _: &str, _: &LimitHit) -> PortResult<()> {
        Ok(())
    }
    fn profiles(&self) -> PortResult<Vec<AccountProfile>> {
        Ok(Vec::new())
    }
}

fn ok_outcome(text: &str) -> AgentOutcome {
    AgentOutcome {
        exit_code: Some(0),
        timed_out: false,
        final_text: text.into(),
        tokens_in: 50,
        tokens_out: 25,
        limit_hit: None,
        infra: None,
        log_tail: String::new(),
    }
}

fn auth_fail_outcome() -> AgentOutcome {
    AgentOutcome {
        exit_code: Some(1),
        timed_out: false,
        final_text: String::new(),
        tokens_in: 10,
        tokens_out: 0,
        limit_hit: None,
        infra: Some(InfraFailure {
            kind: InfraKind::Auth,
            raw: "401 unauthorized".into(),
        }),
        log_tail: "401 unauthorized".into(),
    }
}

struct ScriptedRunner(Mutex<VecDeque<AgentOutcome>>, Mutex<Vec<AgentRequest>>);
impl ScriptedRunner {
    fn from_outcomes(outcomes: Vec<AgentOutcome>) -> Arc<Self> {
        Arc::new(Self(Mutex::new(outcomes.into()), Mutex::new(Vec::new())))
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

struct FakeGit {
    base: PathBuf,
    diff: String,
    integrates: AtomicUsize,
}
impl FakeGit {
    fn new(base: PathBuf, diff: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            base,
            diff: diff.into(),
            integrates: AtomicUsize::new(0),
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
        Ok(self.diff.clone())
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
        Ok(())
    }
    fn is_merged(&self, _: &str) -> PortResult<bool> {
        Ok(false)
    }
    fn cleanup(&self, _: &str) -> PortResult<()> {
        Ok(())
    }
}

fn base_receipt(repo_root: &str, mode: GateMode, status: GateStatus, cfg_hash: &str) -> Receipt {
    Receipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        repo_root: repo_root.into(),
        tree_fp: "git-tree:t1".into(),
        head_sha: Some("head1".into()),
        base_sha: Some("base0".into()),
        mode,
        status,
        reasons: vec![],
        config_hash: cfg_hash.into(),
        commands: vec![],
        static_findings: vec![],
        suppressions: 0,
        head_lock: HeadLockReport {
            status: LockStatus::Clear,
            lock_fp: String::new(),
            locked_changes: vec![],
            approval: None,
        },
        store_compliance: StoreComplianceReport::not_applicable(),
        created_at: 2000,
    }
}

struct ReasonsGate {
    cfg_hash: String,
    reasons: Vec<String>,
}
impl GateRunner for ReasonsGate {
    fn run(&self, tree: &Path) -> PortResult<Receipt> {
        let mut r = base_receipt(
            &tree.to_string_lossy(),
            GateMode::Scoped,
            GateStatus::Clear,
            &self.cfg_hash,
        );
        r.reasons = self.reasons.clone();
        Ok(r)
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

fn sample_spec() -> ImmutableSpec {
    ImmutableSpec {
        description: "Add parser module".into(),
        acceptance: vec!["Parses valid input".into()],
    }
}

fn hooks_stop() -> Hooks<'static> {
    Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    }
}

fn mk_deps(
    tmp: &TempDir,
    db: &Db,
    runner: Arc<dyn AgentRunner>,
    gate: Arc<dyn GateRunner>,
    git: Arc<dyn GitOps>,
    repo: RepoConfig,
) -> Deps {
    Deps {
        store: Arc::new(db.clone()),
        clock: Arc::new(TestClock(AtomicI64::new(2000))),
        runner,
        accounts: Arc::new(EmptyPool),
        git,
        gate,
        notifier: Arc::new(NullNotifier),
        settings: Settings {
            repo_root: tmp.path().to_string_lossy().into_owned(),
            repo,
            max_parallel: 1,
            auto_merge: false,
            sandboxed: false,
            hook_required: false,
            agent_path: std::ffi::OsString::new(),
            mobile: false,
            data_dir: tmp.path().join("data"),
            home: tmp.path().to_path_buf(),
        },
    }
}

#[test]
fn lockfile_summarizer_enforces_exact_allowlist_surfaces_risks_and_valid_hunks() {
    assert!(is_allowlisted_lockfile("Cargo.lock"));
    assert!(is_allowlisted_lockfile("packages/app/package-lock.json"));
    assert!(is_allowlisted_lockfile("ios/Podfile.lock"));
    assert!(is_allowlisted_lockfile("go.sum"));
    assert!(!is_allowlisted_lockfile("src/evil.lock"));
    assert!(!is_allowlisted_lockfile("custom.lock"));
    assert!(!is_allowlisted_lockfile("Cargo.lock.bak"));

    let mut pkg_lines = vec![
        "    \"node_modules/evil-pkg\": {".to_string(),
        "      \"version\": \"1.2.3\",".to_string(),
        "      \"resolved\": \"git+https://attacker.example/evil-pkg.git\",".to_string(),
        "      \"integrity\": \"sha512-deadbeef==\",".to_string(),
        "      \"hasInstallScript\": true,".to_string(),
        "      \"link\": true,".to_string(),
        "      \"bin\": { \"pwn\": \"bin/pwn.js\" }".to_string(),
        "    },".to_string(),
    ];
    for i in 0..80 {
        pkg_lines.push(format!("    \"node_modules/filler-{i}\": {{"));
        pkg_lines.push("      \"version\": \"1.0.0\",".to_string());
        pkg_lines.push(format!(
            "      \"resolved\": \"https://registry.npmjs.org/filler-{i}/-/filler-{i}-1.0.0.tgz\","
        ));
        pkg_lines.push(format!("      \"integrity\": \"sha512-hash{i}==\""));
        pkg_lines.push("    },".to_string());
    }
    let mut diff = format!(
        "diff --git a/package-lock.json b/package-lock.json\n--- a/package-lock.json\n+++ b/package-lock.json\n@@ -1,0 +1,{} @@\n",
        pkg_lines.len()
    );
    for l in &pkg_lines {
        diff.push('+');
        diff.push_str(l);
        diff.push('\n');
    }
    let evil_body = "+backdoor_payload();\n".repeat(300);
    diff.push_str(&format!(
        "diff --git a/src/evil.lock b/src/evil.lock\n--- a/src/evil.lock\n+++ b/src/evil.lock\n@@ -1,0 +1,300 @@\n{evil_body}"
    ));

    let summarized = summarize_diff_lockfiles(&diff);
    assert!(summarized.contains("[lockfile-summary] package-lock.json"));
    assert!(summarized.contains("hasInstallScript"));
    assert!(summarized.contains("git+https://attacker.example/evil-pkg.git"));
    assert!(summarized.contains("[alert:install-script]"));
    assert!(summarized.contains("[alert:non-default-url]"));
    assert!(!summarized.contains("[lockfile-summary] src/evil.lock"));
    assert!(summarized.contains("+backdoor_payload();"));

    let hunks = parse_diff_hunks(&summarized);
    assert!(hunks.contains_key("package-lock.json"));
    assert!(hunks.contains_key("src/evil.lock"));

    let checklist = build_checklist(&sample_spec(), false);
    let answers: Vec<_> = checklist.iter().map(|item| {
        if item.id == "GATE-CI" {
            serde_json::json!({ "id": item.id, "answer": "yes", "evidence": "package-lock.json:2 git+ install script" })
        } else {
            serde_json::json!({ "id": item.id, "answer": "no" })
        }
    }).collect();
    let parsed = validate_verdict_against_diff(
        parse_scored_verdict(
            &serde_json::json!({ "answers": answers }).to_string(),
            &checklist,
            false,
        ),
        &hunks,
    );
    assert!(
        matches!(parsed.verdict, AuditVerdict::Fail { ref findings, .. } if findings.len() == 1)
    );
}

#[test]
fn behavioral_red_unproven_forces_post_gate_audit_even_under_head_lock_scope() {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    let task = Task::new_root(
        "t1",
        tmp.path().to_string_lossy(),
        "Add module",
        sample_spec(),
        Priority::P2,
        1000,
    );
    db.insert_task(&task).unwrap();
    db.set_state("t1", TaskState::Ready, None, 1001).unwrap();

    let mut repo = RepoConfig::default();
    repo.cross_audit.audit_scope = AuditScope::HeadLock;
    repo.cross_audit.score_gate = false;
    let gate = Arc::new(ReasonsGate {
        cfg_hash: aaf_engine::gate::config_hash(&repo),
        reasons: vec!["warning: oracle: behavioral_red_unproven (new module added)".into()],
    });
    let git = FakeGit::new(
        tmp.path().to_path_buf(),
        "diff --git a/src/new_mod.rs b/src/new_mod.rs\n--- /dev/null\n+++ b/src/new_mod.rs\n@@ -0,0 +1,1 @@\n+pub fn parse() -> i32 { 1 }\n",
    );
    let mut forced_spec = sample_spec();
    forced_spec.acceptance.push("extra".into());
    let pass_answers: Vec<_> = build_checklist(&forced_spec, false)
        .iter()
        .map(|i| serde_json::json!({ "id": i.id, "answer": "no" }))
        .collect();
    let pass_json = serde_json::json!({ "answers": pass_answers }).to_string();

    let author_runner = ScriptedRunner::from_outcomes(vec![ok_outcome("implemented")]);
    let audit_runner = ScriptedRunner::from_outcomes(vec![ok_outcome(&pass_json)]);
    let audit_backend = AuditBackend {
        provider: "codex".into(),
        runner: audit_runner.clone(),
        accounts: Arc::new(EmptyPool),
        hook_required: false,
    };
    let deps = mk_deps(&tmp, &db, author_runner, gate, git, repo);
    let summary = run_with_audit(&deps, Some(&audit_backend), Mode::Once, &hooks_stop()).unwrap();
    assert_eq!(summary.review, vec!["t1"]);

    let audit_reqs = audit_runner.1.lock().unwrap();
    assert_eq!(audit_reqs.len(), 1);
    assert!(audit_reqs[0].prompt.contains("behavioral_red_unproven"));
    assert!(
        audit_reqs[0]
            .prompt
            .contains("non-vacuous behavioral assertions")
    );
}

#[test]
fn auditor_retries_up_to_two_inconclusive_and_falls_back_on_secondary_auth() {
    let long_prior = format!(
        "HEAD-MARKER {} TAIL-MARKER",
        "x".repeat(MAX_REPAIR_PRIOR_CHARS + 500)
    );
    let repair_input = aaf_engine::cross_audit::AuditInput {
        spec: sample_spec(),
        diff: String::new(),
        mobile: false,
    };
    let repair_prompt =
        build_format_repair_prompt(&repair_input, false, "no valid audit JSON", &long_prior);
    assert!(repair_prompt.contains("HEAD-MARKER") && repair_prompt.contains("TAIL-MARKER"));

    let tmp = TempDir::new().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    let task = Task::new_root(
        "t1",
        tmp.path().to_string_lossy(),
        "Auth fallback",
        sample_spec(),
        Priority::P2,
        1000,
    );
    db.insert_task(&task).unwrap();
    db.set_state("t1", TaskState::Ready, None, 1001).unwrap();

    let mut repo = RepoConfig::default();
    repo.cross_audit.audit_scope = AuditScope::Always;
    repo.cross_audit.executor = "gemini".into();
    repo.cross_audit.require_different_provider = false;

    let pass_answers: Vec<_> = build_checklist(&sample_spec(), false)
        .iter()
        .map(|i| serde_json::json!({ "id": i.id, "answer": "no" }))
        .collect();
    let pass_json = serde_json::json!({ "answers": pass_answers }).to_string();

    let primary_runner = ScriptedRunner::from_outcomes(vec![
        ok_outcome("author impl"),
        ok_outcome(&long_prior),
        ok_outcome("still invalid json"),
        ok_outcome(&pass_json),
    ]);
    let secondary_runner = ScriptedRunner::from_outcomes(vec![auth_fail_outcome()]);
    let audit_backend = AuditBackend {
        provider: "gemini".into(),
        runner: secondary_runner.clone(),
        accounts: Arc::new(EmptyPool),
        hook_required: false,
    };
    let gate = Arc::new(ReasonsGate {
        cfg_hash: aaf_engine::gate::config_hash(&repo),
        reasons: vec![],
    });
    let git = FakeGit::new(
        tmp.path().to_path_buf(),
        "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,1 +1,2 @@\n fn a() {}\n+fn b() {}\n",
    );
    let deps = mk_deps(&tmp, &db, primary_runner.clone(), gate, git, repo);
    let summary = run_with_audit(&deps, Some(&audit_backend), Mode::Once, &hooks_stop()).unwrap();
    assert!(
        summary.halted.is_none(),
        "must fall back instead of halting: {summary:?}"
    );
    assert_eq!(summary.review, vec!["t1"]);
    assert_eq!(secondary_runner.1.lock().unwrap().len(), 1);
    assert_eq!(primary_runner.1.lock().unwrap().len(), 4);
}
