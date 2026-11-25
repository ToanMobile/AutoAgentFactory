#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Plan v3 Wave 3 Slice 1 tests (part 1 — see `plan_v3_drain_and_land_extra.rs` for part 2):
//! `Mode::Drain` and `LandMode::Auto` fallback behavior.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::config::{LandMode, RepoConfig};
use aaf_domain::gate::{
    GateMode, GateStatus, HeadLockReport, LockStatus, RECEIPT_SCHEMA_VERSION, Receipt,
    StoreComplianceReport,
};
use aaf_domain::ports::{
    AgentOutcome, AgentRequest, AgentRunner, Clock, InfraFailure, InfraKind, LimitHit, PortResult,
    TaskStore,
};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::factory::{
    AccountPool, Deps, GateRunner, GitOps, Grant, Hooks, Integration, Mode, Notifier, Settings, run,
};
use aaf_engine::git::{MergeOutcome, PrOutcome, TaskWorktree};
use aaf_engine::pr_ops::PrMergeOutcome;
use aaf_engine::webhook::WebhookEvent;
use tempfile::TempDir;

struct FixedClock(AtomicI64);
impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct StubPool {
    available: AtomicBool,
    acquire_calls: AtomicU32,
}
impl StubPool {
    fn new(available: bool) -> Self {
        Self {
            available: AtomicBool::new(available),
            acquire_calls: AtomicU32::new(0),
        }
    }
    fn acct() -> AccountProfile {
        AccountProfile {
            id: "acct-1".into(),
            provider: ProviderKind::ClaudeCode,
            config_dir: "/fake/.claude".into(),
            name: "acct-1".into(),
        }
    }
}
impl AccountPool for StubPool {
    fn acquire(&self) -> PortResult<Option<Grant>> {
        self.acquire_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .available
            .load(Ordering::SeqCst)
            .then(|| Grant::new(Some(Self::acct()), None)))
    }
    fn report_limit(&self, _: &str, _: &LimitHit) -> PortResult<()> {
        Ok(())
    }
    fn profiles(&self) -> PortResult<Vec<AccountProfile>> {
        Ok(vec![Self::acct()])
    }
}

struct NullNotifier;
impl Notifier for NullNotifier {
    fn notify(&self, _: &WebhookEvent) -> Result<bool, String> {
        Ok(false)
    }
}

struct ScriptedRunner {
    infra_network: bool,
}
impl AgentRunner for ScriptedRunner {
    fn run(&self, _: &AgentRequest) -> PortResult<AgentOutcome> {
        let infra = self.infra_network.then(|| InfraFailure {
            kind: InfraKind::Network,
            raw: "dns lookup failed".into(),
        });
        Ok(AgentOutcome {
            exit_code: Some(if infra.is_some() { 1 } else { 0 }),
            timed_out: false,
            final_text: if infra.is_some() { "" } else { "done" }.into(),
            tokens_in: 10,
            tokens_out: 5,
            limit_hit: None,
            infra,
            log_tail: String::new(),
        })
    }
}

fn clear_receipt(tree: &Path) -> Receipt {
    Receipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        repo_root: tree.to_string_lossy().into_owned(),
        tree_fp: "git-tree:1111111111111111111111111111111111111111".into(),
        head_sha: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()),
        base_sha: Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into()),
        mode: GateMode::Scoped,
        status: GateStatus::Clear,
        reasons: Vec::new(),
        config_hash: String::new(),
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
        created_at: 1000,
    }
}

struct ClearGate;
impl GateRunner for ClearGate {
    fn run(&self, tree: &Path) -> PortResult<Receipt> {
        Ok(clear_receipt(tree))
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

struct FakeLandGit {
    base: PathBuf,
    merge_outcome: MergeOutcome,
    pr_url: String,
    merge_pr_calls: Mutex<Vec<(String, String)>>,
}
impl FakeLandGit {
    fn new(base: PathBuf, merge_outcome: MergeOutcome, pr_url: &str) -> Self {
        Self {
            base,
            merge_outcome,
            pr_url: pr_url.into(),
            merge_pr_calls: Mutex::new(Vec::new()),
        }
    }
}
impl GitOps for FakeLandGit {
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
        Ok("+line\n".into())
    }
    fn integrate(&self, _: &[String]) -> PortResult<Integration> {
        Ok(Integration {
            tree: TaskWorktree {
                path: self.base.join("batch"),
                branch: "aaf/batch".into(),
            },
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
        Ok("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into())
    }
    fn merge(
        &self,
        _: &TaskWorktree,
        _: &dyn Fn(&Path) -> Result<bool, String>,
    ) -> PortResult<MergeOutcome> {
        Ok(self.merge_outcome.clone())
    }
    fn open_pr(
        &self,
        _: &TaskWorktree,
        _: &str,
        _: &Receipt,
        _: Option<&str>,
    ) -> PortResult<PrOutcome> {
        Ok(PrOutcome::Opened {
            url: self.pr_url.clone(),
        })
    }
    fn merge_pr(
        &self,
        wt: &TaskWorktree,
        pr_url: &str,
        head_sha: &str,
    ) -> PortResult<PrMergeOutcome> {
        self.merge_pr_calls
            .lock()
            .unwrap()
            .push((pr_url.to_string(), head_sha.to_string()));
        Ok(PrMergeOutcome::Merged {
            pr_number: 42,
            head_sha: head_sha.to_string(),
            base_sha: "cccccccccccccccccccccccccccccccccccccccc".into(),
            receipt: Box::new(clear_receipt(&wt.path)),
            local_sync_note: "synced".into(),
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

fn add_ready(db: &Db, id: &str) {
    let spec = ImmutableSpec {
        description: format!("spec {id}"),
        acceptance: vec!["passes".into()],
    };
    let mut task = Task::new_root(
        id,
        "/fake/repo",
        format!("task {id}"),
        spec,
        Priority::P2,
        100,
    );
    task.state = TaskState::Ready;
    db.insert_task(&task).unwrap();
}

fn make_deps(
    tmp: &TempDir,
    db: &Db,
    pool: Arc<dyn AccountPool>,
    infra_network: bool,
    git: Arc<dyn GitOps>,
    repo_cfg: RepoConfig,
) -> Deps {
    Deps {
        store: Arc::new(db.clone()),
        clock: Arc::new(FixedClock(AtomicI64::new(1000))),
        runner: Arc::new(ScriptedRunner { infra_network }),
        accounts: pool,
        git,
        gate: Arc::new(ClearGate),
        notifier: Arc::new(NullNotifier),
        settings: Settings {
            repo_root: "/fake/repo".into(),
            auto_merge: repo_cfg.run.auto_merge,
            repo: repo_cfg,
            max_parallel: 2,
            sandboxed: false,
            hook_required: false,
            agent_path: Default::default(),
            mobile: false,
            data_dir: tmp.path().to_path_buf(),
            home: tmp.path().join("home"),
            owner_github_allowlist: Vec::new(),
        },
    }
}

#[test]
fn mode_drain_exits_without_busy_spin_on_work_exhausted_and_review_cap() {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    add_ready(&db, "t1");
    let pool = Arc::new(StubPool::new(false));
    let git = Arc::new(FakeLandGit::new(
        tmp.path().to_path_buf(),
        MergeOutcome::Merged {
            new_base_sha: "b1".into(),
        },
        "https://github.com/org/repo/pull/42",
    ));
    let deps = make_deps(
        &tmp,
        &db,
        pool.clone(),
        false,
        git.clone(),
        RepoConfig::default(),
    );
    let hooks = Hooks {
        idle: &|| panic!("Mode::Drain must not invoke Hooks::idle"),
        reap: &|_| Ok(()),
    };
    let t0 = Instant::now();
    let s1 = run(&deps, Mode::Drain, &hooks).unwrap();
    assert!(t0.elapsed() < Duration::from_secs(2), "busy-spun");
    assert_eq!(s1.agent_runs, 0);
    assert_eq!(s1.waiting_for_account, 1);
    assert_eq!(pool.acquire_calls.load(Ordering::SeqCst), 1);

    add_ready(&db, "t2");
    let mut cfg = RepoConfig::default();
    cfg.run.max_open_review = 1;
    let deps_cap = make_deps(&tmp, &db, Arc::new(StubPool::new(true)), false, git, cfg);
    let t1 = Instant::now();
    let s2 = run(&deps_cap, Mode::Drain, &hooks).unwrap();
    assert!(t1.elapsed() < Duration::from_secs(2), "busy-spun on cap");
    assert_eq!(s2.review.len(), 1);
    assert!(s2.waiting_for_review_cap >= 1);
    assert_eq!(s2.waiting_for_account, 0);
}

#[test]
fn mode_drain_halts_fail_fast_on_transient_network_error() {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    add_ready(&db, "t-net");
    let git = Arc::new(FakeLandGit::new(
        tmp.path().to_path_buf(),
        MergeOutcome::Merged {
            new_base_sha: "b1".into(),
        },
        "https://github.com/org/repo/pull/42",
    ));
    let deps = make_deps(
        &tmp,
        &db,
        Arc::new(StubPool::new(true)),
        true,
        git,
        RepoConfig::default(),
    );
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };
    let summary = run(&deps, Mode::Drain, &hooks).unwrap();
    assert!(
        summary.halted.is_some(),
        "Mode::Drain must halt: {summary:?}"
    );
    assert_eq!(db.get_task("t-net").unwrap().state, TaskState::Ready);
}

#[test]
fn land_auto_falls_back_to_pr_merge_on_push_rejected_and_review_on_dirty_base() {
    let tmp = TempDir::new().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    add_ready(&db, "t-prot");
    let mut cfg = RepoConfig::default();
    cfg.run.land = LandMode::Auto;
    cfg.run.auto_merge = true;

    let git_prot = Arc::new(FakeLandGit::new(
        tmp.path().to_path_buf(),
        MergeOutcome::PushRejected {
            reason: "GH006: Protected branch update failed".into(),
        },
        "https://github.com/org/repo/pull/42",
    ));
    let deps_prot = make_deps(
        &tmp,
        &db,
        Arc::new(StubPool::new(true)),
        false,
        git_prot.clone(),
        cfg.clone(),
    );
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };
    let s = run(&deps_prot, Mode::Drain, &hooks).unwrap();
    assert_eq!(s.done, vec!["t-prot".to_string()]);
    assert_eq!(db.get_task("t-prot").unwrap().state, TaskState::Done);
    assert_eq!(git_prot.merge_pr_calls.lock().unwrap().len(), 1);

    add_ready(&db, "t-dirty");
    let git_dirty = Arc::new(FakeLandGit::new(
        tmp.path().to_path_buf(),
        MergeOutcome::BaseCheckoutDirty {
            path: PathBuf::from("/fake/repo"),
        },
        "https://github.com/org/repo/pull/43",
    ));
    let deps_dirty = make_deps(
        &tmp,
        &db,
        Arc::new(StubPool::new(true)),
        false,
        git_dirty,
        cfg,
    );
    let sd = run(&deps_dirty, Mode::Drain, &hooks).unwrap();
    assert_eq!(sd.review, vec!["t-dirty".to_string()]);
    assert!(sd.blocked.is_empty());
    assert_eq!(db.get_task("t-dirty").unwrap().state, TaskState::Review);
}
