#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for Round 5A Lane 3 (`linux_sandbox`, pre/post audit reuse, `scheduler`).

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aaf_domain::account::AccountProfile;
use aaf_domain::config::{AuditScope, RepoConfig};
use aaf_domain::event::EventKind;
use aaf_domain::gate::{
    ChangeKind, GateMode, GateStatus, HeadLockReport, LockStatus, LockedChange,
    RECEIPT_SCHEMA_VERSION, Receipt, Severity, StaticFinding, StoreComplianceReport, StoreStatus,
};
use aaf_domain::ports::{
    AgentOutcome, AgentRequest, AgentRunner, Clock, EventSink, LimitHit, PortResult, TaskStore,
};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::cross_audit::build_checklist;
use aaf_engine::db::Db;
use aaf_engine::factory::{
    AccountPool, AuditBackend, Deps, GateRunner, GitOps, Grant, Hooks, Integration, Mode, Notifier,
    RunSummary, Settings, run, run_with_audit,
};
use aaf_engine::gate;
use aaf_engine::git::{MergeOutcome, PrOutcome, TaskWorktree};
use aaf_engine::webhook::WebhookEvent;
use tempfile::TempDir;

// ---- Shared factory test fakes --------------------------------------------

struct TestClock(AtomicI64);
impl Clock for TestClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct NullDeps;
impl Notifier for NullDeps {
    fn notify(&self, _: &WebhookEvent) -> Result<bool, String> {
        Ok(false)
    }
}
impl AccountPool for NullDeps {
    fn acquire(&self) -> PortResult<Option<Grant>> {
        Ok(Some(Grant::new(None, None)))
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
        tokens_in: 40,
        tokens_out: 20,
        limit_hit: None,
        infra: None,
        log_tail: String::new(),
    }
}

struct QueueRunner(Mutex<VecDeque<AgentOutcome>>, AtomicUsize);
impl QueueRunner {
    fn from_texts(texts: &[&str]) -> Arc<Self> {
        let q = texts.iter().map(|s| ok_outcome(s)).collect();
        Arc::new(Self(Mutex::new(q), AtomicUsize::new(0)))
    }
}
impl AgentRunner for QueueRunner {
    fn run(&self, _: &AgentRequest) -> PortResult<AgentOutcome> {
        self.1.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| ok_outcome("ok")))
    }
}

fn spec() -> ImmutableSpec {
    ImmutableSpec {
        description: "Round 5 factory tests".into(),
        acceptance: vec!["invariants hold".into()],
    }
}

fn high_scores_json() -> String {
    let ans: Vec<_> = build_checklist(&spec(), false)
        .iter()
        .map(|i| serde_json::json!({ "id": i.id, "answer": "no" }))
        .collect();
    let sc: Vec<_> = [
        ("G1", 98, "nit"),
        ("G2", 100, ""),
        ("G3", 96, "edge"),
        ("G4", 100, ""),
        ("G5", 95, "alloc"),
        ("G6", 97, "style"),
        ("G7", 100, ""),
    ]
    .iter()
    .map(|(id, s, r)| serde_json::json!({ "id": id, "score": s, "deduction_reason": r }))
    .collect();
    serde_json::json!({ "answers": ans, "scores": sc }).to_string()
}

#[derive(Default)]
struct FakeGit {
    base: PathBuf,
    diffs: AtomicUsize,
    integrates: AtomicUsize,
    merged_review: AtomicBool,
}
impl GitOps for FakeGit {
    fn task_worktree(&self, id: &str) -> PortResult<TaskWorktree> {
        let path = self.base.join(format!("wt-{id}"));
        fs::create_dir_all(&path).unwrap();
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
        self.diffs.fetch_add(1, Ordering::SeqCst);
        Ok("diff --git a/src/lib.rs b/src/lib.rs\n+pub fn ok() {}\n".into())
    }
    fn integrate(&self, ids: &[String]) -> PortResult<Integration> {
        let n = self.integrates.fetch_add(1, Ordering::SeqCst);
        let path = self.base.join(format!("clean-{n}"));
        fs::create_dir_all(&path).unwrap();
        Ok(Integration {
            tree: TaskWorktree {
                path,
                branch: format!("aaf/clean-{n}"),
            },
            merged: ids.to_vec(),
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
    fn is_merged(&self, task_id: &str) -> PortResult<bool> {
        Ok(task_id == "t_rev" && self.merged_review.load(Ordering::SeqCst))
    }
    fn cleanup(&self, _: &str) -> PortResult<()> {
        Ok(())
    }
}

#[derive(Default)]
struct HeadLockGate {
    data_dir: PathBuf,
    cfg_hash: String,
    lock_fp: String,
    binding: Option<(String, Option<String>)>,
    receipt_tree_fp: String,
    receipt_base_sha: Option<String>,
    findings: Vec<StaticFinding>,
    unresolved: Vec<String>,
}
impl GateRunner for HeadLockGate {
    fn run(&self, tree: &Path) -> PortResult<Receipt> {
        let store_compliance = if self.unresolved.is_empty() {
            StoreComplianceReport::not_applicable()
        } else {
            StoreComplianceReport {
                status: StoreStatus::Clear,
                critical: 0,
                warnings: 0,
                unresolved: self.unresolved.clone(),
                findings: Vec::new(),
            }
        };
        Ok(Receipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            repo_root: tree.to_string_lossy().into_owned(),
            tree_fp: self.receipt_tree_fp.clone(),
            head_sha: Some("head1".into()),
            base_sha: self.receipt_base_sha.clone(),
            mode: GateMode::Scoped,
            status: GateStatus::Clear,
            reasons: Vec::new(),
            config_hash: self.cfg_hash.clone(),
            commands: Vec::new(),
            static_findings: self.findings.clone(),
            suppressions: 0,
            head_lock: self.head_lock(tree)?,
            store_compliance,
            created_at: 2000,
        })
    }
    fn head_lock(&self, tree: &Path) -> PortResult<HeadLockReport> {
        let approved = !self.lock_fp.is_empty()
            && aaf_fp::load_key(&self.data_dir)
                .ok()
                .and_then(|k| {
                    aaf_fp::read_approval(&self.data_dir, &k, tree, &self.lock_fp)
                        .ok()
                        .flatten()
                })
                .is_some();
        let clear = self.lock_fp.is_empty() || approved;
        let (status, locked_changes) = if clear {
            (LockStatus::Clear, Vec::new())
        } else {
            (
                LockStatus::Blocked,
                vec![LockedChange {
                    path: "Cargo.toml".into(),
                    change: ChangeKind::Modified,
                    tier: aaf_domain::gate::LockTier::Hard,
                }],
            )
        };
        Ok(HeadLockReport {
            status,
            lock_fp: self.lock_fp.clone(),
            locked_changes,
            approval: None,
        })
    }
    fn verify_cached_receipt(&self, _: &Path, receipt: &Receipt) -> bool {
        receipt.status == GateStatus::Clear
    }
    fn tree_binding(&self, _: &Path) -> Option<(String, Option<String>)> {
        self.binding.clone()
    }
}

struct Env {
    tmp: TempDir,
    db: Db,
    clock: Arc<TestClock>,
    git: Arc<FakeGit>,
}
impl Env {
    fn new(now: i64) -> Self {
        let tmp = TempDir::new().unwrap();
        let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
        let git = Arc::new(FakeGit {
            base: tmp.path().to_path_buf(),
            ..FakeGit::default()
        });
        Self {
            tmp,
            db,
            clock: Arc::new(TestClock(AtomicI64::new(now))),
            git,
        }
    }
    fn add(&self, id: &str, state: TaskState) {
        let mut t = Task::new_root(id, "/fake/repo", id, spec(), Priority::P1, 1);
        t.state = state;
        self.db.insert_task(&t).unwrap();
    }
    fn deps(
        &self,
        runner: Arc<dyn AgentRunner>,
        gate_runner: Arc<dyn GateRunner>,
        repo: RepoConfig,
        max_parallel: u32,
    ) -> Deps {
        Deps {
            store: Arc::new(self.db.clone()),
            clock: self.clock.clone(),
            runner,
            accounts: Arc::new(NullDeps),
            git: self.git.clone(),
            gate: gate_runner,
            notifier: Arc::new(NullDeps),
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
                owner_github_allowlist: Vec::new(),
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

fn run_head_lock_case(
    receipt_tree_fp: &str,
    receipt_base_sha: Option<&str>,
    findings: Vec<StaticFinding>,
    unresolved: Vec<String>,
) -> (Env, Arc<QueueRunner>, String, RunSummary) {
    let mut repo = RepoConfig::default();
    repo.run.executor = "claude".into();
    repo.cross_audit.audit_scope = AuditScope::Always;
    repo.cross_audit.score_gate = true;
    repo.cross_audit.min_average_score = 95;
    repo.cross_audit.min_criterion_score = 90;
    let cfg_hash = gate::config_hash(&repo);
    let pass = high_scores_json();
    let env = Env::new(2000);
    env.add("t1", TaskState::Ready);
    let gate_runner = Arc::new(HeadLockGate {
        data_dir: env.tmp.path().to_path_buf(),
        cfg_hash: cfg_hash.clone(),
        lock_fp: "lock-fp-1".into(),
        binding: Some(("fp-match".into(), Some("base0".into()))),
        receipt_tree_fp: receipt_tree_fp.into(),
        receipt_base_sha: receipt_base_sha.map(str::to_string),
        findings,
        unresolved,
    });
    let deps = env.deps(QueueRunner::from_texts(&["impl"]), gate_runner, repo, 1);
    let ar = QueueRunner::from_texts(&[&pass, &pass]);
    let audit = AuditBackend {
        provider: "gemini".into(),
        runner: ar.clone(),
        accounts: Arc::new(NullDeps),
        hook_required: false,
    };
    let summary = run_with_audit(&deps, Some(&audit), Mode::Once, &hooks_stop()).unwrap();
    (env, ar, cfg_hash, summary)
}

// ---- 2. Pre-gate / post-gate cross-audit unification tests ----------------

#[test]
fn pre_gate_audit_reuses_scores_post_gate_and_still_enforces_mechanical_score() {
    // Case A: matching (tree_fp, base_sha) + clean mechanical score -> 1 LLM turn, signed verdict.
    let (env, ar, cfg_hash, s) =
        run_head_lock_case("fp-match", Some("base0"), Vec::new(), Vec::new());
    assert_eq!((s.review, s.gate_runs), (vec!["t1".to_string()], 1));
    assert_eq!(ar.1.load(Ordering::SeqCst), 1);
    assert_eq!(env.git.integrates.load(Ordering::SeqCst), 1);
    assert_eq!(env.git.diffs.load(Ordering::SeqCst), 1);
    let key = aaf_fp::load_key(env.tmp.path()).unwrap();
    let wt = env.tmp.path().join("wt-t1");
    let rec = aaf_fp::read_audit_verdict(env.tmp.path(), &key, &wt, "fp-match", &cfg_hash)
        .unwrap()
        .expect("signed post-gate audit verdict must exist");
    assert_eq!(
        (
            rec.verdict.as_str(),
            rec.mechanical_score,
            rec.llm_average,
            rec.effective_score
        ),
        ("PASS", 100, Some(98), 98)
    );
    assert!(env.db.events_for_task("t1", 50).unwrap().iter().any(|e| {
        e.kind == EventKind::CrossAudit && e.payload_json.contains("\"phase\":\"post_gate\"")
    }));

    // Case B: matching (tree_fp, base_sha) but low mechanical_score (91 < 95) -> still fails gate!
    let warn = |rule: &str| StaticFinding {
        rule: rule.into(),
        severity: Severity::Warn,
        file: "src/lib.rs".into(),
        line: Some(12),
        message: "warn".into(),
        fix: "fix it".into(),
    };
    let (env_b, ar_b, cfg_b, sb) = run_head_lock_case(
        "fp-match",
        Some("base0"),
        vec![warn("RUST-001"), warn("RUST-002"), warn("RUST-003")],
        vec!["camera permission".into()],
    );
    assert!(sb.review.is_empty() && sb.requeued == vec!["t1"], "{sb:?}");
    assert_eq!(ar_b.1.load(Ordering::SeqCst), 1);
    assert_eq!(env_b.git.integrates.load(Ordering::SeqCst), 1);
    let p = env_b.tmp.path();
    assert!(!aaf_fp::audit_verdict_path(p, &p.join("wt-t1"), "fp-match", &cfg_b).exists());
    let notes = env_b.db.get_task("t1").unwrap().context_notes;
    assert!(
        notes
            .iter()
            .any(|n| n.starts_with("audit_rework:") && n.contains("RUST-001"))
    );
}

#[test]
fn pre_gate_audit_mismatch_on_tree_fp_or_base_sha_fails_closed_to_full_post_audit() {
    for (label, receipt_fp, receipt_base) in [
        ("tree_fp_drift", "fp-drifted", Some("base0")),
        ("base_sha_drift", "fp-match", Some("base-drifted")),
    ] {
        let (env, ar, cfg_hash, s) =
            run_head_lock_case(receipt_fp, receipt_base, Vec::new(), Vec::new());
        assert_eq!(s.review, vec!["t1"], "{label}: {s:?}");
        assert_eq!(ar.1.load(Ordering::SeqCst), 2, "{label}: fail closed");
        assert_eq!(env.git.integrates.load(Ordering::SeqCst), 2, "{label}");
        let key = aaf_fp::load_key(env.tmp.path()).unwrap();
        let wt = env.tmp.path().join("wt-t1");
        assert!(
            aaf_fp::read_audit_verdict(env.tmp.path(), &key, &wt, receipt_fp, &cfg_hash)
                .unwrap()
                .is_some(),
            "{label}: signed verdict for receipt tree_fp must be written"
        );
    }
}

// ---- 3. Scheduler Mode::Auto review-dep waiter sweep test -----------------

struct SiblingWaitRunner {
    clock: Arc<TestClock>,
    git: Arc<FakeGit>,
    sib_active: AtomicBool,
    down_started_while_sib_active: AtomicBool,
}
impl AgentRunner for SiblingWaitRunner {
    fn run(&self, req: &AgentRequest) -> PortResult<AgentOutcome> {
        if req.cwd.ends_with("wt-t_sib") {
            self.sib_active.store(true, Ordering::SeqCst);
            self.git.merged_review.store(true, Ordering::SeqCst);
            self.clock.0.store(1035, Ordering::SeqCst);
            let t0 = Instant::now();
            while !self.down_started_while_sib_active.load(Ordering::SeqCst)
                && t0.elapsed() < Duration::from_secs(8)
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            self.sib_active.store(false, Ordering::SeqCst);
            return Ok(ok_outcome("sib done"));
        }
        if req.cwd.ends_with("wt-t_down") && self.sib_active.load(Ordering::SeqCst) {
            self.down_started_while_sib_active
                .store(true, Ordering::SeqCst);
        }
        Ok(ok_outcome("down done"))
    }
}

#[test]
fn auto_mode_sweeps_review_dep_waiter_while_sibling_active() {
    let env = Env::new(1000);
    env.add("t_rev", TaskState::Review);
    env.add("t_down", TaskState::Ready);
    env.add("t_sib", TaskState::Ready);
    env.db.add_dep("t_down", "t_rev").unwrap();

    let repo = RepoConfig::default();
    let gate_runner = Arc::new(HeadLockGate {
        data_dir: env.tmp.path().to_path_buf(),
        cfg_hash: gate::config_hash(&repo),
        receipt_tree_fp: "fp-clear".into(),
        receipt_base_sha: Some("base0".into()),
        ..HeadLockGate::default()
    });
    let runner = Arc::new(SiblingWaitRunner {
        clock: env.clock.clone(),
        git: env.git.clone(),
        sib_active: AtomicBool::new(false),
        down_started_while_sib_active: AtomicBool::new(false),
    });
    let deps = env.deps(runner.clone(), gate_runner, repo, 2);

    let summary = run(&deps, Mode::Auto, &hooks_stop()).unwrap();
    assert!(
        runner.down_started_while_sib_active.load(Ordering::SeqCst),
        "{summary:?}"
    );
    assert_eq!(summary.done, vec!["t_rev"]);
    let mut rev = summary.review;
    rev.sort();
    assert_eq!(rev, vec!["t_down", "t_sib"]);
}
