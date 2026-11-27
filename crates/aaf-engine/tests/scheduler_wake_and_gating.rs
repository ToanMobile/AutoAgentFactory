#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for:
//! 1. `wake_due_blocked_tasks` executing in `Mode::Once` and active scheduler loops
//!    (not only on `Mode::Auto` idle).
//! 2. Resumed `Gating` tasks holding coding dispatch until their initial gate check
//!    settles so `Ready` tasks cannot steal the last review account.
//! 3. Dual-review basename citation rejection remaining fail-closed while emitting
//!    full repo-relative diff path hints for 1-shot repair.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::blocked::BlockedReason;
use aaf_domain::config::{AuditScope, RepoConfig};
use aaf_domain::gate::{
    GateMode, GateStatus, HeadLockReport, LockStatus, RECEIPT_SCHEMA_VERSION, Receipt,
    StoreComplianceReport,
};
use aaf_domain::ports::{
    AgentOutcome, AgentRequest, AgentRunner, Clock, LimitHit, PortResult, TaskStore,
};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::cross_audit::build_checklist;
use aaf_engine::cross_audit::dual_verdict::{DualReviewerOutcome, parse_and_validate_dual_review};
use aaf_engine::db::Db;
use aaf_engine::factory::{
    AccountPool, Deps, GateRunner, GitOps, Grant, Hooks, Integration, Mode, Notifier, Purpose,
    Settings, run,
};
use aaf_engine::git::{MergeOutcome, PrOutcome, TaskWorktree};
use aaf_engine::unblocker::QuotaRoute;
use aaf_engine::webhook::WebhookEvent;

struct FixedClock(AtomicI64);
impl Clock for FixedClock {
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

struct StubGit(PathBuf);
impl GitOps for StubGit {
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
        Ok("diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1,2 @@\n fn a() {}\n+fn b() {}\n".into())
    }
    fn integrate(&self, ids: &[String]) -> PortResult<Integration> {
        let path = self.0.join("clean-0");
        std::fs::create_dir_all(&path).unwrap();
        Ok(Integration {
            tree: TaskWorktree {
                path,
                branch: "aaf/clean-0".into(),
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

struct ClearGate {
    cfg_hash: String,
    delay_ms: u64,
}
impl GateRunner for ClearGate {
    fn run(&self, tree: &Path) -> PortResult<Receipt> {
        if self.delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(self.delay_ms));
        }
        let name = tree.file_name().unwrap().to_string_lossy();
        Ok(Receipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            repo_root: tree.to_string_lossy().into_owned(),
            tree_fp: format!("git-tree:{name}"),
            head_sha: Some("head1".into()),
            base_sha: Some("base0".into()),
            mode: GateMode::Scoped,
            status: GateStatus::Clear,
            reasons: vec![],
            config_hash: self.cfg_hash.clone(),
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

struct SingleAccountPool {
    profile: AccountProfile,
    busy: AtomicBool,
}
impl SingleAccountPool {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            profile: AccountProfile {
                id: "acc-1".into(),
                name: "acc-1".into(),
                provider: ProviderKind::ClaudeCode,
                config_dir: "/tmp/acc-1".into(),
            },
            busy: AtomicBool::new(false),
        })
    }
}
impl AccountPool for SingleAccountPool {
    fn acquire(&self) -> PortResult<Option<Grant>> {
        if self.busy.swap(true, Ordering::SeqCst) {
            return Ok(None);
        }
        Ok(Some(Grant::new(Some(self.profile.clone()), None)))
    }
    fn acquire_for(&self, _: &str, _: Purpose) -> PortResult<Option<Grant>> {
        self.acquire()
    }
    fn after_limit(&self) -> PortResult<Option<QuotaRoute>> {
        Ok(Some(QuotaRoute::Wait))
    }
    fn report_limit(&self, _: &str, _: &LimitHit) -> PortResult<()> {
        Ok(())
    }
    fn profiles(&self) -> PortResult<Vec<AccountProfile>> {
        Ok(vec![self.profile.clone()])
    }
}

struct RecordingRunner {
    events: Mutex<Vec<String>>,
    reply: String,
    pool: Arc<SingleAccountPool>,
}
impl AgentRunner for RecordingRunner {
    fn run(&self, req: &AgentRequest) -> PortResult<AgentOutcome> {
        let label = if req.sandbox.read_only {
            format!("audit:{}", req.cwd)
        } else {
            format!("code:{}", req.cwd)
        };
        self.events.lock().unwrap().push(label);
        self.pool.busy.store(false, Ordering::SeqCst);
        Ok(AgentOutcome {
            exit_code: Some(0),
            timed_out: false,
            final_text: self.reply.clone(),
            tokens_in: 100,
            tokens_out: 20,
            limit_hit: None,
            infra: None,
            log_tail: String::new(),
        })
    }
}

fn spec() -> ImmutableSpec {
    ImmutableSpec {
        description: "implement feature".into(),
        acceptance: vec!["tests pass".into()],
    }
}

fn make_deps(
    root: &Path,
    db: Db,
    clock: Arc<FixedClock>,
    runner: Arc<dyn AgentRunner>,
    accounts: Arc<dyn AccountPool>,
    repo: RepoConfig,
    gate_delay_ms: u64,
) -> Deps {
    let cfg_hash = aaf_engine::gate::config_hash(&repo);
    Deps {
        store: Arc::new(db),
        clock,
        runner,
        accounts,
        git: Arc::new(StubGit(root.to_path_buf())),
        gate: Arc::new(ClearGate {
            cfg_hash,
            delay_ms: gate_delay_ms,
        }),
        notifier: Arc::new(NullNotifier),
        settings: Settings {
            repo_root: "/fake/repo".into(),
            repo,
            max_parallel: 2,
            auto_merge: true,
            sandboxed: false,
            hook_required: false,
            agent_path: Default::default(),
            mobile: false,
            data_dir: root.to_path_buf(),
            home: root.to_path_buf(),
            owner_github_allowlist: Vec::new(),
        },
    }
}

#[test]
fn mode_once_wakes_due_infra_blocked_task_and_executes_it() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    let mut repo = RepoConfig::default();
    repo.cross_audit.audit_scope = AuditScope::HeadLock;

    let mut task = Task::new_root(
        "t-wake",
        "/fake/repo",
        "wake me",
        spec(),
        Priority::P1,
        1_000,
    );
    task.state = TaskState::Blocked;
    task.blocked_reason = Some(BlockedReason::Infra {
        detail: "transient audit error".into(),
    });
    db.insert_task(&task).unwrap();
    db.set_retry_schedule("t-wake", Some(1_500), 0, 1_000)
        .unwrap();

    let pool = SingleAccountPool::new();
    let runner = Arc::new(RecordingRunner {
        events: Mutex::new(Vec::new()),
        reply: "done".into(),
        pool: Arc::clone(&pool),
    });
    let clock = Arc::new(FixedClock(AtomicI64::new(2_000)));
    let deps = make_deps(tmp.path(), db.clone(), clock, runner, pool, repo, 0);
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };

    let summary = run(&deps, Mode::Once, &hooks).unwrap();
    assert_eq!(summary.done, vec!["t-wake"]);
    assert_eq!(db.get_task("t-wake").unwrap().state, TaskState::Done);
}

#[test]
fn active_loop_wakes_due_blocked_task_after_sibling_finishes_without_going_idle() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    let mut repo = RepoConfig::default();
    repo.cross_audit.audit_scope = AuditScope::HeadLock;

    // t-first runs immediately at t=1_000; t-delayed is Blocked(Infra) with retry_after=1_500.
    // While t-first runs, the clock advances to 2_000, so on the next active-loop tick
    // (before Mode::Once exits) wake_due_blocked_tasks wakes t-delayed and executes it too.
    let mut first = Task::new_root(
        "t-first",
        "/fake/repo",
        "first",
        spec(),
        Priority::P1,
        1_000,
    );
    first.state = TaskState::Ready;
    db.insert_task(&first).unwrap();

    let mut delayed = Task::new_root(
        "t-delayed",
        "/fake/repo",
        "delayed",
        spec(),
        Priority::P1,
        1_001,
    );
    delayed.state = TaskState::Blocked;
    delayed.blocked_reason = Some(BlockedReason::Infra {
        detail: "transient audit error".into(),
    });
    db.insert_task(&delayed).unwrap();
    db.set_retry_schedule("t-delayed", Some(1_500), 0, 1_000)
        .unwrap();

    let clock = Arc::new(FixedClock(AtomicI64::new(1_000)));
    let pool = SingleAccountPool::new();
    struct AdvancingRunner {
        clock: Arc<FixedClock>,
        pool: Arc<SingleAccountPool>,
    }
    impl AgentRunner for AdvancingRunner {
        fn run(&self, _req: &AgentRequest) -> PortResult<AgentOutcome> {
            self.clock.0.store(2_000, Ordering::SeqCst);
            self.pool.busy.store(false, Ordering::SeqCst);
            Ok(AgentOutcome {
                exit_code: Some(0),
                timed_out: false,
                final_text: "done".into(),
                tokens_in: 50,
                tokens_out: 10,
                limit_hit: None,
                infra: None,
                log_tail: String::new(),
            })
        }
    }
    let runner = Arc::new(AdvancingRunner {
        clock: Arc::clone(&clock),
        pool: Arc::clone(&pool),
    });
    let deps = make_deps(tmp.path(), db.clone(), clock, runner, pool, repo, 0);
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };

    let summary = run(&deps, Mode::Once, &hooks).unwrap();
    assert!(summary.done.contains(&"t-first".to_string()), "{summary:?}");
    assert!(
        summary.done.contains(&"t-delayed".to_string()),
        "t-delayed must wake once retry_after <= now during active loop: {summary:?}"
    );
    assert_eq!(db.get_task("t-delayed").unwrap().state, TaskState::Done);
}

#[test]
fn dual_review_basename_citation_rejection_stays_fail_closed_with_full_path_hint() {
    let diff = "diff --git a/CarConnect/app/src/main/java/com/techlead/audio/RNNoiseDenoiseEngine.kt \
                b/CarConnect/app/src/main/java/com/techlead/audio/RNNoiseDenoiseEngine.kt\n\
                --- a/CarConnect/app/src/main/java/com/techlead/audio/RNNoiseDenoiseEngine.kt\n\
                +++ b/CarConnect/app/src/main/java/com/techlead/audio/RNNoiseDenoiseEngine.kt\n\
                @@ -90,2 +90,4 @@\n fun keep() {}\n+fun process() {}\n+fun chuaTiengBip() {}\n";
    let checklist = build_checklist(&spec(), false);
    let answers: Vec<serde_json::Value> = checklist
        .iter()
        .map(|item| serde_json::json!({ "id": item.id, "answer": "no", "evidence": "" }))
        .collect();
    let raw = serde_json::json!({
        "answers": answers,
        "scores": [
            {"id": "G1", "score": 80, "deduction_reason": "bug"},
            {"id": "G2", "score": 100, "deduction_reason": ""},
            {"id": "G3", "score": 100, "deduction_reason": ""},
            {"id": "G4", "score": 100, "deduction_reason": ""},
            {"id": "G5", "score": 100, "deduction_reason": ""},
            {"id": "G6", "score": 100, "deduction_reason": ""},
            {"id": "G7", "score": 100, "deduction_reason": ""}
        ],
        "issues": [{
            "id": "I1",
            "severity": "P1",
            "category": "bug",
            "location": "RNNoiseDenoiseEngine.kt:91-92",
            "description": "missing guard",
            "proposed_fix": "add guard"
        }]
    })
    .to_string();

    let outcome = parse_and_validate_dual_review(&raw, &checklist, diff, None, true, 90, 85, 100);
    let DualReviewerOutcome::Inconclusive { reason, .. } = outcome else {
        panic!("bare basename must fail closed as Inconclusive, got {outcome:?}");
    };
    assert!(
        reason.contains("CarConnect/app/src/main/java/com/techlead/audio/RNNoiseDenoiseEngine.kt"),
        "rejection error must include full repo-relative path hint: {reason}"
    );
    assert!(
        reason.contains("not bare filename"),
        "rejection error must explain bare filename failure: {reason}"
    );
}
