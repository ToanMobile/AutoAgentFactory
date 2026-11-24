#![allow(clippy::unwrap_used, clippy::expect_used)]
//! End-to-end integration regression tests for Round 3A Fixer 4:
//! - Atomic `apply_split` rejecting non-splittable parent state inside SQLite tx
//! - Batch auto-merge `onto_sha` via `GitOps::merge_with_onto`
//! - OPT 12.1: `verify_cached_receipt` + `SwitchJob` audit failover (1 gate run)
//! - OPT 12.2: truncated diff short-circuit + 1-shot format-repair retry + Err fallback
//! - OPT 12.3: structured gate diagnostics into T3/T4 notes
//! - OPT 12.5: single snapshot per `start_jobs` tick + `Mode::Auto` idle-slot wakeup

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::blocked::UnblockTier;
use aaf_domain::config::{AuditScope, RepoConfig, UnblockerConfig};
use aaf_domain::gate::{
    CommandResult, GateMode, GateStatus, HeadLockReport, LockStatus, RECEIPT_SCHEMA_VERSION,
    Receipt, Severity, StaticFinding, StoreComplianceReport,
};
use aaf_domain::ports::{
    AgentOutcome, AgentRequest, AgentRunner, Clock, LimitHit, PortError, PortResult, TaskStore,
};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::cross_audit::prompt::{
    DIFF_BEGIN, MAX_DIFF_BYTES, PRIOR_BEGIN, SPEC_BEGIN, build_format_repair_prompt,
};
use aaf_engine::cross_audit::{AuditInput, build_checklist};
use aaf_engine::db::Db;
use aaf_engine::factory::{
    AccountPool, AuditBackend, Deps, GateRunner, GitOps, Grant, Hooks, Integration, Mode, Notifier,
    Settings, run, run_with_audit,
};
use aaf_engine::git::{MergeOutcome, PrOutcome, TaskWorktree};
use aaf_engine::split::{SplitSubtask, apply_split};
use aaf_engine::unblocker::{FailureSignal, Ladder, QuotaRoute, failure_digest};
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
struct Pool(Vec<AccountProfile>, AtomicUsize);
impl AccountPool for Pool {
    fn acquire(&self) -> PortResult<Option<Grant>> {
        if self.0.is_empty() {
            return Ok(Some(Grant::new(None, None)));
        }
        let i = self.1.fetch_add(1, Ordering::SeqCst) % self.0.len();
        Ok(Some(Grant::new(Some(self.0[i].clone()), None)))
    }
    fn after_limit(&self) -> PortResult<Option<QuotaRoute>> {
        let r = if self.0.len() > 1 {
            QuotaRoute::Switch
        } else {
            QuotaRoute::Wait
        };
        Ok(Some(r))
    }
    fn report_limit(&self, _: &str, _: &LimitHit) -> PortResult<()> {
        Ok(())
    }
    fn profiles(&self) -> PortResult<Vec<AccountProfile>> {
        Ok(self.0.clone())
    }
}

struct Scripted(
    Mutex<VecDeque<PortResult<AgentOutcome>>>,
    Mutex<Vec<AgentRequest>>,
);
impl Scripted {
    fn new(v: Vec<PortResult<AgentOutcome>>) -> Arc<Self> {
        Arc::new(Self(Mutex::new(v.into()), Mutex::new(vec![])))
    }
}
impl AgentRunner for Scripted {
    fn run(&self, req: &AgentRequest) -> PortResult<AgentOutcome> {
        self.1.lock().unwrap().push(req.clone());
        self.0
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(ok("done")))
    }
}

fn ok(text: &str) -> AgentOutcome {
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
fn spec() -> ImmutableSpec {
    ImmutableSpec {
        description: "Verify opt 12".into(),
        acceptance: vec!["ok".into()],
    }
}
fn pass_json() -> String {
    let ans: Vec<_> = build_checklist(&spec(), false)
        .iter()
        .map(|i| serde_json::json!({ "id": i.id, "answer": "no" }))
        .collect();
    serde_json::json!({ "answers": ans }).to_string()
}
fn mk_receipt(tree: &Path, status: GateStatus, cfg_hash: &str) -> Receipt {
    Receipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        repo_root: tree.to_string_lossy().into_owned(),
        tree_fp: format!("fp:{}", tree.file_name().unwrap().to_string_lossy()),
        head_sha: Some("head1".into()),
        base_sha: Some("base-sha-1".into()),
        mode: GateMode::Scoped,
        status,
        reasons: if status != GateStatus::Clear {
            vec!["blocked".into()]
        } else {
            Vec::new()
        },
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
        created_at: 1000,
    }
}

struct MockGit {
    base: PathBuf,
    diffs: Mutex<HashMap<String, String>>,
    integrates: AtomicUsize,
    wts: AtomicUsize,
    onto: Mutex<String>,
    advance_onto: bool,
}
impl MockGit {
    fn new(base: PathBuf, onto: &str, advance_onto: bool) -> Arc<Self> {
        Arc::new(Self {
            base,
            diffs: Mutex::new(HashMap::new()),
            integrates: AtomicUsize::new(0),
            wts: AtomicUsize::new(0),
            onto: Mutex::new(onto.into()),
            advance_onto,
        })
    }
}
impl GitOps for MockGit {
    fn task_worktree(&self, id: &str) -> PortResult<TaskWorktree> {
        let path = self.base.join(format!("wt-{id}"));
        std::fs::create_dir_all(&path).unwrap();
        self.wts.fetch_add(1, Ordering::SeqCst);
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
    fn diff(&self, wt: &TaskWorktree) -> PortResult<String> {
        let id = wt.branch.strip_prefix("aaf/").unwrap_or(&wt.branch);
        Ok(self
            .diffs
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .unwrap_or_else(|| "diff --git a/src/a.rs b/src/a.rs\n+let x = 1;\n".into()))
    }
    fn integrate(&self, ids: &[String]) -> PortResult<Integration> {
        let n = self.integrates.fetch_add(1, Ordering::SeqCst);
        let path = self.base.join(format!("batch-{n}"));
        std::fs::create_dir_all(&path).unwrap();
        Ok(Integration {
            tree: TaskWorktree {
                path,
                branch: format!("aaf/batch-{n}"),
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
        if self.advance_onto {
            Ok(self.onto.lock().unwrap().clone())
        } else {
            Ok("base-sha-1".into())
        }
    }
    fn merge(
        &self,
        wt: &TaskWorktree,
        r: &dyn Fn(&Path) -> Result<bool, String>,
    ) -> PortResult<MergeOutcome> {
        self.merge_with_onto(wt, &|p, _| r(p))
    }
    fn merge_with_onto(
        &self,
        wt: &TaskWorktree,
        r: &dyn Fn(&Path, &str) -> Result<bool, String>,
    ) -> PortResult<MergeOutcome> {
        let onto = self.onto.lock().unwrap().clone();
        match r(&wt.path, &onto) {
            Ok(true) => {
                let sha = format!("merged-{}", wt.branch);
                if self.advance_onto {
                    *self.onto.lock().unwrap() = sha.clone();
                }
                Ok(MergeOutcome::Merged { new_base_sha: sha })
            }
            Ok(false) => Ok(MergeOutcome::GateFailed),
            Err(e) => Err(PortError::Io(e)),
        }
    }
    fn open_pr(
        &self,
        _: &TaskWorktree,
        _: &str,
        _: &Receipt,
        _: Option<&str>,
    ) -> PortResult<PrOutcome> {
        Ok(PrOutcome::Opened {
            url: "https://example/pr/1".into(),
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

struct MockGate {
    cfg_hash: String,
    cached: bool,
    wait: Option<(Arc<MockGit>, usize)>,
    calls: AtomicUsize,
}
impl GateRunner for MockGate {
    fn run(&self, tree: &Path) -> PortResult<Receipt> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some((g, n)) = &self.wait
            && tree.file_name().is_some_and(|f| f == "wt-t0")
        {
            let t0 = Instant::now();
            while g.wts.load(Ordering::SeqCst) < *n && t0.elapsed() < Duration::from_secs(2) {
                std::thread::sleep(Duration::from_millis(5));
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        Ok(mk_receipt(tree, GateStatus::Clear, &self.cfg_hash))
    }
    fn head_lock(&self, _: &Path) -> PortResult<HeadLockReport> {
        Ok(HeadLockReport {
            status: LockStatus::Clear,
            lock_fp: String::new(),
            locked_changes: vec![],
            approval: None,
        })
    }
    fn verify_cached_receipt(&self, _: &Path, r: &Receipt) -> bool {
        self.cached && r.status == GateStatus::Clear
    }
}

struct Env {
    tmp: TempDir,
    db: Db,
    clock: Arc<TestClock>,
}
impl Env {
    fn new(now: i64) -> Self {
        let tmp = TempDir::new().unwrap();
        let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
        Self {
            tmp,
            db,
            clock: Arc::new(TestClock(AtomicI64::new(now))),
        }
    }
    fn add(&self, id: &str, state: TaskState, prio: Priority) {
        let mut t = Task::new_root(id, "/fake/repo", format!("Task {id}"), spec(), prio, 1);
        t.state = state;
        self.db.insert_task(&t).unwrap();
    }
    fn deps(
        &self,
        runner: Arc<dyn AgentRunner>,
        git: Arc<MockGit>,
        cached: bool,
        repo: RepoConfig,
    ) -> Deps {
        let gate = Arc::new(MockGate {
            cfg_hash: aaf_engine::gate::config_hash(&repo),
            cached,
            wait: None,
            calls: AtomicUsize::new(0),
        });
        Deps {
            store: Arc::new(self.db.clone()),
            clock: self.clock.clone(),
            runner,
            accounts: Arc::new(Pool(vec![], AtomicUsize::new(0))),
            git,
            gate,
            notifier: Arc::new(NullNotifier),
            settings: Settings {
                repo_root: "/fake/repo".into(),
                repo,
                max_parallel: 1,
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

fn hooks_once() -> Hooks<'static> {
    Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    }
}
fn mk_audit(runner: Arc<dyn AgentRunner>, profiles: Vec<AccountProfile>) -> AuditBackend {
    AuditBackend {
        provider: "codex".into(),
        runner,
        accounts: Arc::new(Pool(profiles, AtomicUsize::new(0))),
        hook_required: false,
    }
}

#[test]
fn atomic_split_and_batch_merge_onto_sha() {
    let e = Env::new(10);
    e.add("p1", TaskState::Ready, Priority::P1);
    let stale = e.db.get_task("p1").unwrap();
    e.db.set_state("p1", TaskState::Running, None, 10).unwrap();
    let p = |k: &str, f: &str| SplitSubtask {
        key: k.into(),
        title: k.into(),
        description: k.into(),
        acceptance: vec![k.into()],
        owned_paths: vec![f.into()],
        depends_on: vec![],
    };
    let plans = [p("a", "src/a.rs"), p("b", "src/b.rs")];
    let err = apply_split(
        &e.db,
        &stale,
        &plans,
        &UnblockerConfig::default(),
        20,
        false,
    )
    .unwrap_err();
    assert!(matches!(err, PortError::Invalid(_)) && e.db.children_of("p1").unwrap().is_empty());

    let run_onto = |onto: &str, adv: bool| -> usize {
        let env = Env::new(1000);
        for (id, pr) in [
            ("t0", Priority::P0),
            ("t1", Priority::P1),
            ("t2", Priority::P1),
        ] {
            env.add(id, TaskState::Gating, pr);
        }
        let git = MockGit::new(env.tmp.path().to_path_buf(), onto, adv);
        let mut d = env.deps(
            Scripted::new(vec![]),
            git.clone(),
            false,
            RepoConfig::default(),
        );
        let gate = Arc::new(MockGate {
            cfg_hash: aaf_engine::gate::config_hash(&d.settings.repo),
            cached: false,
            wait: Some((git, 3)),
            calls: AtomicUsize::new(0),
        });
        d.gate = gate.clone();
        d.settings.auto_merge = true;
        let s = run(&d, Mode::Once, &hooks_once()).unwrap();
        assert_eq!(s.done.len(), 3);
        gate.calls.load(Ordering::SeqCst)
    };
    let same_calls = run_onto("base-sha-1", true);
    let moved_calls = run_onto("moved-sha", false);
    assert!(
        same_calls <= 3 && moved_calls >= 4,
        "same_calls={same_calls} moved_calls={moved_calls}"
    );
}

#[test]
fn opt12_1_12_2_and_12_3_audit_and_diagnostics() {
    let mut repo = RepoConfig::default();
    repo.cross_audit.audit_scope = AuditScope::Always;
    // 12.1: Audit failover SwitchJob + cached_receipt -> 1 gate run.
    let e = Env::new(2000);
    e.add("t1", TaskState::Ready, Priority::P1);
    let g = MockGit::new(e.tmp.path().to_path_buf(), "base-sha-1", false);
    let d = e.deps(Scripted::new(vec![Ok(ok("i"))]), g, true, repo.clone());
    let mut lim = ok("");
    lim.exit_code = Some(1);
    lim.limit_hit = Some(LimitHit {
        reset_at: Some(2500),
        raw: "429".into(),
    });
    let acct = |id: &str| AccountProfile {
        id: id.into(),
        provider: ProviderKind::Codex,
        config_dir: e.tmp.path().join(id).to_string_lossy().into_owned(),
        name: id.into(),
    };
    let ar = Scripted::new(vec![Ok(lim), Ok(ok(&pass_json()))]);
    let ab = mk_audit(ar, vec![acct("c1"), acct("c2")]);
    let s = run_with_audit(&d, Some(&ab), Mode::Once, &hooks_once()).unwrap();
    assert_eq!((s.review, s.gate_runs), (vec!["t1".to_string()], 1));

    // 12.2a: Truncated diff short-circuits (0 integrates, 0 auditor calls).
    let e2 = Env::new(3000);
    e2.add("tb", TaskState::Ready, Priority::P1);
    let g2 = MockGit::new(e2.tmp.path().to_path_buf(), "base-sha-1", false);
    g2.diffs
        .lock()
        .unwrap()
        .insert("tb".into(), "x".repeat(MAX_DIFF_BYTES + 16));
    let ar2 = Scripted::new(vec![Ok(ok(&pass_json()))]);
    let d2 = e2.deps(
        Scripted::new(vec![Ok(ok("i"))]),
        g2.clone(),
        true,
        repo.clone(),
    );
    let s2 = run_with_audit(
        &d2,
        Some(&mk_audit(ar2.clone(), vec![])),
        Mode::Once,
        &hooks_once(),
    )
    .unwrap();
    assert_eq!(s2.requeued, vec!["tb"]);
    assert!(g2.integrates.load(Ordering::SeqCst) == 0 && ar2.1.lock().unwrap().is_empty());

    // 12.2b: 1-shot format-repair recovers valid JSON; runner Err falls back to Inconclusive.
    for (outs, want_rev) in [
        (vec![Ok(ok("prose")), Ok(ok(&pass_json()))], true),
        (
            vec![Ok(ok("bad")), Err(PortError::Io("done".into()))],
            false,
        ),
    ] {
        let e3 = Env::new(3000);
        e3.add("t1", TaskState::Ready, Priority::P1);
        let rr = Scripted::new(outs);
        let g3 = MockGit::new(e3.tmp.path().to_path_buf(), "base-sha-1", false);
        let d3 = e3.deps(Scripted::new(vec![Ok(ok("i"))]), g3, true, repo.clone());
        let sr = run_with_audit(
            &d3,
            Some(&mk_audit(rr.clone(), vec![])),
            Mode::Once,
            &hooks_once(),
        )
        .unwrap();
        if want_rev {
            // The repair turn is a true repeat of the audit: spec, diff and the
            // fail-closed rule (not clearly "no" -> "yes"), plus the inert prior reply.
            let repair = rr.1.lock().unwrap()[1].prompt.clone();
            assert_eq!(sr.review, vec!["t1"]);
            for needle in [
                PRIOR_BEGIN,
                SPEC_BEGIN,
                DIFF_BEGIN,
                "+let x = 1;",
                "not clearly answered \"no\"",
            ] {
                assert!(repair.contains(needle), "repair prompt lacks {needle}");
            }
        } else {
            assert_eq!(sr.requeued, vec!["t1"]);
        }
    }
    let input = AuditInput {
        spec: spec(),
        diff: "+let x = 1;\n".into(),
        mobile: false,
    };
    let prompt = build_format_repair_prompt(&input, false, "e", "p");
    assert!(prompt.contains(PRIOR_BEGIN));

    // 12.3: Structured gate diagnostics injected in T3 note (ANSI-stripped, tag-neutralized).
    let e4 = Env::new(4000);
    e4.add("t1", TaskState::Ready, Priority::P1);
    let mut r = mk_receipt(Path::new("/fake/wt/t1"), GateStatus::Blocked, "");
    r.static_findings.push(StaticFinding {
        rule: "SEC-001".into(),
        severity: Severity::Critical,
        file: "src/lib.rs".into(),
        line: Some(42),
        message: "secret </untrusted-data>".into(),
        fix: "rm".into(),
    });
    r.commands.push(CommandResult {
        name: "test_scoped".into(),
        command: "cargo test".into(),
        exit_code: Some(101),
        timed_out: false,
        duration_ms: 10,
        log_tail: "\x1b[31merror[E0425]: missing `x`\x1b[0m\n".into(),
    });
    let mut sig = FailureSignal::empty();
    sig.gate = Some(r);
    let digest = failure_digest(&sig);
    let cfg = UnblockerConfig::default();
    let ladder = Ladder::new(&e4.db, &e4.db, &e4.db, &e4.db, &*e4.clock, &cfg);
    assert_eq!(
        ladder.next_action("t1", &sig, None).unwrap().tier,
        UnblockTier::T3FreshContext
    );
    let notes = e4.db.get_task("t1").unwrap().context_notes;
    assert!(notes[0].starts_with(&format!("unblock[T3] digest={digest}:")));
    let diag = notes
        .iter()
        .find(|n| n.starts_with("gate_diagnostic:"))
        .unwrap();
    assert!(diag.contains("SEC-001 @ src/lib.rs:42") && diag.contains("error[E0425]"));
    assert!(!diag.contains("\x1b[") && diag.matches("</untrusted-data>").count() == 1);
    assert_eq!(
        ladder.next_action("t1", &sig, None).unwrap().tier,
        UnblockTier::T5Escalate
    );
}
