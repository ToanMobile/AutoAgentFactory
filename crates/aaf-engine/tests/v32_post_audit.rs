#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Phase 2 v3.2 tests: `is_large_diff` heuristics and end-to-end post-gate audit.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::config::{AuditScope, RepoConfig};
use aaf_domain::event::EventKind;
use aaf_domain::gate::{
    GateMode, GateStatus, HeadLockReport, LockStatus, RECEIPT_SCHEMA_VERSION, Receipt,
    StoreComplianceReport,
};
use aaf_domain::ports::{
    AccountStore, AgentOutcome, AgentRequest, AgentRunner, BudgetStore, Clock, EventSink, LimitHit,
    PortResult, TaskStore,
};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::cross_audit::build_checklist;
use aaf_engine::db::Db;
use aaf_engine::factory::{
    AccountPool, AuditBackend, Deps, GateRunner, GitOps, Grant, Hooks, Integration, Mode, Notifier,
    Settings, is_large_diff, run_with_audit,
};
use aaf_engine::gate;
use aaf_engine::git::{MergeOutcome, PrOutcome, TaskWorktree};
use aaf_engine::webhook::WebhookEvent;
use tempfile::TempDir;

// ---- 1. is_large_diff heuristics ------------------------------------------

fn fake_file_diff(path: &str, added: usize) -> String {
    let mut out =
        format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n");
    for i in 0..added {
        out.push_str(&format!("+let line_{i} = {i};\n"));
    }
    out
}

#[test]
fn is_large_diff_checks_file_count_prod_loc_and_truncation() {
    let three_files = format!(
        "{}{}{}",
        fake_file_diff("src/a.rs", 1),
        fake_file_diff("src/b.rs", 1),
        fake_file_diff("src/c.rs", 1),
    );
    assert!(is_large_diff(&three_files, false));

    let over_200 = format!(
        "{}{}",
        fake_file_diff("src/a.rs", 101),
        fake_file_diff("src/b.rs", 100)
    );
    assert!(is_large_diff(&over_200, false));

    let small = format!(
        "{}{}",
        fake_file_diff("src/a.rs", 25),
        fake_file_diff("src/b.rs", 25)
    );
    assert!(!is_large_diff(&small, false));

    let mostly_tests = format!(
        "{}{}",
        fake_file_diff("src/a.rs", 50),
        fake_file_diff("tests/a_test.rs", 300)
    );
    assert!(!is_large_diff(&mostly_tests, false));

    assert!(is_large_diff(&small, true));
}

// ---- 4. End-to-end post-gate audit & restart idempotency ------------------

struct FixedClock(i64);
impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0
    }
}

struct FixedPool(Vec<AccountProfile>);
impl AccountPool for FixedPool {
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

struct NullNotifier;
impl Notifier for NullNotifier {
    fn notify(&self, _: &WebhookEvent) -> Result<bool, String> {
        Ok(false)
    }
}

fn ok_outcome(text: &str) -> AgentOutcome {
    AgentOutcome {
        exit_code: Some(0),
        timed_out: false,
        final_text: text.to_string(),
        tokens_in: 40,
        tokens_out: 20,
        limit_hit: None,
        infra: None,
        log_tail: String::new(),
    }
}

struct QueueRunner {
    outcomes: Mutex<VecDeque<AgentOutcome>>,
    calls: AtomicUsize,
}

impl QueueRunner {
    fn from_texts(texts: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            outcomes: Mutex::new(texts.iter().map(|s| ok_outcome(s)).collect()),
            calls: AtomicUsize::new(0),
        })
    }
}

impl AgentRunner for QueueRunner {
    fn run(&self, _: &AgentRequest) -> PortResult<AgentOutcome> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let next = self.outcomes.lock().unwrap().pop_front();
        Ok(next.unwrap_or_else(|| ok_outcome("ok")))
    }
}

struct PostAuditGit {
    base: PathBuf,
    counter: AtomicUsize,
    pr_notes: Mutex<Vec<Option<String>>>,
}

impl GitOps for PostAuditGit {
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
        Ok(fake_file_diff("src/lib.rs", 10))
    }
    fn integrate(&self, _: &[String]) -> PortResult<Integration> {
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        let tree = TaskWorktree {
            path: self.base.join(format!("clean-{n}")),
            branch: format!("aaf/clean-{n}"),
        };
        Ok(Integration {
            tree,
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
        note: Option<&str>,
    ) -> PortResult<PrOutcome> {
        self.pr_notes.lock().unwrap().push(note.map(str::to_string));
        Ok(PrOutcome::Opened {
            url: "https://example.test/pr/42".to_string(),
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

struct ClearGate {
    config_hash: String,
}

impl GateRunner for ClearGate {
    fn run(&self, tree: &Path) -> PortResult<Receipt> {
        let name = tree.file_name().unwrap().to_string_lossy().to_string();
        Ok(Receipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            repo_root: tree.to_string_lossy().into_owned(),
            tree_fp: format!("git-tree:{name}"),
            head_sha: Some("deadbeef".to_string()),
            base_sha: Some("base0".to_string()),
            mode: GateMode::Scoped,
            status: GateStatus::Clear,
            reasons: Vec::new(),
            config_hash: self.config_hash.clone(),
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
            created_at: 2000,
        })
    }
    fn head_lock(&self, _: &Path) -> PortResult<HeadLockReport> {
        Ok(HeadLockReport {
            status: LockStatus::Clear,
            lock_fp: String::new(),
            locked_changes: Vec::new(),
            approval: None,
        })
    }
}

fn sample_spec() -> ImmutableSpec {
    ImmutableSpec {
        description: "Add post-gate audit scoring".to_string(),
        acceptance: vec!["score gate enforces thresholds".to_string()],
    }
}

fn scored_pass_json(spec: &ImmutableSpec, scores: [(&str, u8, &str); 7]) -> String {
    let answers: Vec<serde_json::Value> = build_checklist(spec, false)
        .iter()
        .map(|item| serde_json::json!({ "id": item.id, "answer": "no" }))
        .collect();
    let score_json: Vec<serde_json::Value> = scores
        .iter()
        .map(|(id, score, reason)| {
            serde_json::json!({ "id": id, "score": score, "deduction_reason": reason })
        })
        .collect();
    serde_json::json!({ "answers": answers, "scores": score_json }).to_string()
}

fn setup_world(
    audit_response: &str,
) -> (
    TempDir,
    Db,
    Arc<PostAuditGit>,
    Arc<QueueRunner>,
    Deps,
    AuditBackend,
) {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    let mut task = Task::new_root(
        "t1",
        "/fake/repo",
        "Task t1",
        sample_spec(),
        Priority::P1,
        1,
    );
    task.state = TaskState::Ready;
    db.insert_task(&task).unwrap();

    let acct = AccountProfile {
        id: "claude-1".to_string(),
        provider: ProviderKind::ClaudeCode,
        config_dir: tmp.path().join("claude-1").to_string_lossy().into_owned(),
        name: "claude-1".to_string(),
    };
    db.upsert_account(&acct).unwrap();

    let mut repo = RepoConfig::default();
    repo.run.executor = "claude".to_string();
    repo.cross_audit.audit_scope = AuditScope::Always;
    repo.cross_audit.score_gate = true;
    repo.cross_audit.min_average_score = 95;
    repo.cross_audit.min_criterion_score = 90;
    let config_hash = gate::config_hash(&repo);

    let git = Arc::new(PostAuditGit {
        base: tmp.path().to_path_buf(),
        counter: AtomicUsize::new(0),
        pr_notes: Mutex::new(Vec::new()),
    });
    let deps = Deps {
        store: Arc::new(db.clone()),
        clock: Arc::new(FixedClock(2000)),
        runner: QueueRunner::from_texts(&["implemented"]),
        accounts: Arc::new(FixedPool(vec![acct])),
        git: git.clone(),
        gate: Arc::new(ClearGate { config_hash }),
        notifier: Arc::new(NullNotifier),
        settings: Settings {
            repo_root: "/fake/repo".to_string(),
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
    let audit_runner = QueueRunner::from_texts(&[audit_response]);
    let audit = AuditBackend {
        provider: "gemini".to_string(),
        runner: audit_runner.clone(),
        accounts: Arc::new(FixedPool(Vec::new())),
        hook_required: false,
    };
    (tmp, db, git, audit_runner, deps, audit)
}

#[test]
fn post_gate_audit_writes_signed_verdict_and_survives_restart_without_reauditing() {
    let pass_json = scored_pass_json(
        &sample_spec(),
        [
            ("G1", 98, "minor nit"),
            ("G2", 100, ""),
            ("G3", 96, "edge comment"),
            ("G4", 100, ""),
            ("G5", 95, "small alloc"),
            ("G6", 97, "style"),
            ("G7", 100, ""),
        ],
    );
    let (tmp, db, git, audit_runner, deps, audit) = setup_world(&pass_json);
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };

    let summary = run_with_audit(&deps, Some(&audit), Mode::Once, &hooks).unwrap();
    assert_eq!(summary.review, vec!["t1"]);
    assert!(summary.blocked.is_empty(), "{summary:?}");
    assert_eq!(audit_runner.calls.load(Ordering::SeqCst), 1);

    let key = aaf_fp::load_key(tmp.path()).unwrap();
    let wt = tmp.path().join("wt-t1");
    let cfg_hash = gate::config_hash(&deps.settings.repo);
    let record = aaf_fp::read_audit_verdict(tmp.path(), &key, &wt, "git-tree:wt-t1", &cfg_hash)
        .unwrap()
        .expect("signed post-gate audit verdict must be written");
    assert_eq!(record.verdict, "PASS");
    assert_eq!(record.mechanical_score, 100);
    assert_eq!(record.llm_average, Some(98));
    assert_eq!(record.effective_score, 98);
    assert!(!record.same_account);

    let events = db.events_for_task("t1", 50).unwrap();
    let gate_ev = events
        .iter()
        .find(|e| e.kind == EventKind::GateResult)
        .unwrap();
    assert!(gate_ev.payload_json.contains("\"mechanical_score\":100"));
    assert!(gate_ev.payload_json.contains("\"config_hash\""));
    let audit_ev = events
        .iter()
        .find(|e| e.kind == EventKind::CrossAudit)
        .unwrap();
    assert!(audit_ev.payload_json.contains("\"phase\":\"post_gate\""));
    assert!(audit_ev.payload_json.contains("\"effective_score\":98"));

    let notes = git.pr_notes.lock().unwrap();
    assert!(
        notes
            .first()
            .and_then(|n| n.as_deref())
            .is_some_and(|n| n.contains("## Handover"))
    );
    drop(notes);

    // Restart while task is still in Gating: signed verdict skips re-auditing.
    db.set_state("t1", TaskState::Gating, None, 2001).unwrap();
    let resumed = run_with_audit(&deps, Some(&audit), Mode::Once, &hooks).unwrap();
    assert_eq!(resumed.review, vec!["t1"]);
    assert_eq!(audit_runner.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn post_gate_low_score_routes_to_audit_rework_without_writing_pass_verdict() {
    let low_score_json = scored_pass_json(
        &sample_spec(),
        [
            ("G1", 98, "minor nit"),
            ("G2", 100, ""),
            ("G3", 96, "edge note"),
            ("G4", 82, "missing regression test for boundary"),
            ("G5", 98, "ok"),
            ("G6", 98, "ok"),
            ("G7", 98, "ok"),
        ],
    );
    let (tmp, db, _git, audit_runner, deps, audit) = setup_world(&low_score_json);
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };

    let summary = run_with_audit(&deps, Some(&audit), Mode::Once, &hooks).unwrap();
    assert_eq!(summary.requeued, vec!["t1"]);
    assert!(summary.review.is_empty(), "{summary:?}");
    assert_eq!(audit_runner.calls.load(Ordering::SeqCst), 1);

    let wt = tmp.path().join("wt-t1");
    let cfg_hash = gate::config_hash(&deps.settings.repo);
    assert!(!aaf_fp::audit_verdict_path(tmp.path(), &wt, "git-tree:wt-t1", &cfg_hash).exists());

    let task = db.get_task("t1").unwrap();
    assert_eq!(task.state, TaskState::Ready);
    assert!(
        task.context_notes.iter().any(|n| {
            n.starts_with("audit_rework:")
                && n.contains("missing regression test for boundary")
                && n.contains("<untrusted-data id=\"audit-findings\">")
        }),
        "{:?}",
        task.context_notes
    );
    // Audit rework is decoupled from unblocker cycles (Plan v3).
    assert_eq!(db.get_budget("t1").unwrap().cycles_used, 0);
}
