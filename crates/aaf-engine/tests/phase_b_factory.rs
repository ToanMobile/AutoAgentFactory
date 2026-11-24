//! Integration tests for Phase B1, B2, and B2b:
//! - B1: Pre-gate rebase for solo/`alone` jobs in `auto_merge` mode (1 gate run
//!   instead of 2; 0 gate runs on rebase conflict; re-gate fallback if base moves
//!   after rebase; skipped when `proof.required = true`).
//! - B2: Sequential `alone` land-before-next-rebase in `auto_merge`, and parallel
//!   `alone` gating + streaming answers in PR mode.
//! - B2b: `RealGate::rerun_failed_test` early-breaks on the first passing rerun.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::config::{MachineConfig, RepoConfig};
use aaf_domain::event::EventKind;
use aaf_domain::gate::{
    CommandResult, GateMode, GateStatus, HeadLockReport, LockStatus, RECEIPT_SCHEMA_VERSION,
    Receipt, StoreComplianceReport,
};
use aaf_domain::ports::{
    AgentOutcome, AgentRequest, AgentRunner, Clock, EventSink, LimitHit, PortError, PortResult,
    TaskStore,
};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::factory::adapters::{RealGate, SystemClock};
use aaf_engine::factory::{
    AccountPool, Deps, GateRunner, GitOps, Grant, Hooks, Integration, Mode, Notifier, RunSummary,
    Settings, run,
};
use aaf_engine::git::{MergeOutcome, PrOutcome, TaskWorktree};
use aaf_engine::unblocker::RerunResult;
use aaf_engine::webhook::WebhookEvent;

const REPO: &str = "/fake/repo";
const TIMEOUT: Duration = Duration::from_secs(5);

struct FixedClock(AtomicI64);
impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct SilentNotifier;
impl Notifier for SilentNotifier {
    fn notify(&self, _: &WebhookEvent) -> Result<bool, String> {
        Ok(false)
    }
}

struct FreePool(Vec<AccountProfile>);
impl AccountPool for FreePool {
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

struct GatedRunner(Option<Arc<(Mutex<bool>, Condvar)>>);
impl AgentRunner for GatedRunner {
    fn run(&self, _: &AgentRequest) -> PortResult<AgentOutcome> {
        if let Some(flag) = &self.0 {
            let guard = flag.0.lock().unwrap();
            let _ = flag.1.wait_timeout_while(guard, TIMEOUT, |ready| !*ready);
        }
        Ok(AgentOutcome {
            exit_code: Some(0),
            timed_out: false,
            final_text: "done".into(),
            tokens_in: 10,
            tokens_out: 5,
            limit_hit: None,
            infra: None,
            log_tail: String::new(),
        })
    }
}

#[derive(Default)]
struct GitState {
    base: u32,
    rebase_conflicts: HashMap<String, Vec<String>>,
    events: Vec<String>,
    bump_before_merge: HashSet<String>,
}

struct RecordingGit {
    root: PathBuf,
    state: Mutex<GitState>,
}
impl RecordingGit {
    fn new(root: PathBuf, base: u32) -> Arc<Self> {
        Arc::new(Self {
            root,
            state: Mutex::new(GitState {
                base,
                ..GitState::default()
            }),
        })
    }
    fn task_of(path: &Path) -> String {
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

impl GitOps for RecordingGit {
    fn task_worktree(&self, id: &str) -> PortResult<TaskWorktree> {
        Ok(TaskWorktree {
            path: self.root.join(id),
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
        Ok("diff --git a/src/lib.rs b/src/lib.rs\n+x\n".into())
    }
    fn integrate(&self, ids: &[String]) -> PortResult<Integration> {
        Ok(Integration {
            tree: TaskWorktree {
                path: self.root.join("batch"),
                branch: "aaf/batch".into(),
            },
            merged: ids.to_vec(),
            conflicted: Vec::new(),
        })
    }
    fn discard(&self, _: &Integration) -> PortResult<()> {
        Ok(())
    }
    fn rebase(&self, wt: &TaskWorktree) -> PortResult<Vec<String>> {
        let id = Self::task_of(&wt.path);
        let mut s = self.state.lock().unwrap();
        let b = s.base;
        s.events.push(format!("rebase:{id}@base{b}"));
        Ok(s.rebase_conflicts.get(&id).cloned().unwrap_or_default())
    }
    fn base_sha(&self) -> PortResult<String> {
        Ok(format!("base{}", self.state.lock().unwrap().base))
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
        regate: &dyn Fn(&Path, &str) -> Result<bool, String>,
    ) -> PortResult<MergeOutcome> {
        let id = Self::task_of(&wt.path);
        let onto = {
            let mut s = self.state.lock().unwrap();
            if s.bump_before_merge.remove(&id) {
                s.base += 100;
            }
            format!("base{}", s.base)
        };
        if !regate(&wt.path, &onto).map_err(PortError::Io)? {
            return Ok(MergeOutcome::GateFailed);
        }
        let mut s = self.state.lock().unwrap();
        s.base += 1;
        let new_base = format!("base{}", s.base);
        s.events.push(format!("merge:{id}@{new_base}"));
        Ok(MergeOutcome::Merged {
            new_base_sha: new_base,
        })
    }
    fn open_pr(
        &self,
        wt: &TaskWorktree,
        _: &str,
        _: &Receipt,
        _: Option<&str>,
    ) -> PortResult<PrOutcome> {
        let id = Self::task_of(&wt.path);
        self.state.lock().unwrap().events.push(format!("pr:{id}"));
        Ok(PrOutcome::Opened {
            url: format!("https://example.invalid/pr/{id}"),
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

struct RecordingGate {
    git: Arc<RecordingGit>,
    approved_alone: HashSet<String>,
    red_tasks: HashSet<String>,
    calls: AtomicU32,
    on_run: Box<dyn Fn(&str) + Send + Sync>,
}

fn mk_receipt(
    tree: &Path,
    status: GateStatus,
    reasons: Vec<String>,
    base_sha: Option<String>,
    commands: Vec<CommandResult>,
) -> Receipt {
    Receipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        repo_root: tree.to_string_lossy().into_owned(),
        tree_fp: format!("fp:{}", RecordingGit::task_of(tree)),
        head_sha: None,
        base_sha,
        mode: GateMode::Scoped,
        status,
        reasons,
        config_hash: String::new(),
        commands,
        static_findings: Vec::new(),
        suppressions: 0,
        head_lock: HeadLockReport {
            status: LockStatus::Clear,
            lock_fp: String::new(),
            locked_changes: Vec::new(),
            approval: None,
        },
        store_compliance: StoreComplianceReport::not_applicable(),
        created_at: 1_000,
    }
}

impl GateRunner for RecordingGate {
    fn run(&self, tree: &Path) -> PortResult<Receipt> {
        let id = RecordingGit::task_of(tree);
        self.calls.fetch_add(1, Ordering::SeqCst);
        {
            let mut s = self.git.state.lock().unwrap();
            let b = s.base;
            s.events.push(format!("gate:{id}@base{b}"));
        }
        (self.on_run)(&id);
        let (status, reasons) = if self.red_tasks.contains(&id) {
            (GateStatus::Blocked, vec!["lint failed".into()])
        } else {
            (GateStatus::Clear, Vec::new())
        };
        Ok(mk_receipt(
            tree,
            status,
            reasons,
            self.git.base_sha().ok(),
            Vec::new(),
        ))
    }
    fn head_lock(&self, tree: &Path) -> PortResult<HeadLockReport> {
        let status = if self.approved_alone.contains(&RecordingGit::task_of(tree)) {
            LockStatus::Approved
        } else {
            LockStatus::Clear
        };
        Ok(HeadLockReport {
            status,
            lock_fp: String::new(),
            locked_changes: Vec::new(),
            approval: None,
        })
    }
}

fn add_task(db: &Db, id: &str, state: TaskState, priority: Priority) {
    let mut t = Task::new_root(
        id,
        REPO,
        format!("task {id}"),
        ImmutableSpec {
            description: "do work".into(),
            acceptance: vec!["ok".into()],
        },
        priority,
        1,
    );
    t.state = state;
    db.insert_task(&t).unwrap();
}

fn make_deps(
    tmp: &Path,
    db: &Db,
    git: Arc<RecordingGit>,
    gate: Arc<RecordingGate>,
    runner: Arc<dyn AgentRunner>,
    auto_merge: bool,
) -> Deps {
    Deps {
        store: Arc::new(db.clone()),
        clock: Arc::new(FixedClock(AtomicI64::new(1_000))),
        runner,
        accounts: Arc::new(FreePool(vec![AccountProfile {
            id: "a1".into(),
            provider: ProviderKind::ClaudeCode,
            config_dir: tmp.join("a1").to_string_lossy().into_owned(),
            name: "a1".into(),
        }])),
        git,
        gate,
        notifier: Arc::new(SilentNotifier),
        settings: Settings {
            repo_root: REPO.into(),
            repo: RepoConfig::default(),
            max_parallel: 1,
            auto_merge,
            sandboxed: false,
            hook_required: false,
            agent_path: Default::default(),
            mobile: false,
            data_dir: tmp.to_path_buf(),
            home: tmp.to_path_buf(),
        },
    }
}

fn hooks_once() -> Hooks<'static> {
    Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    }
}

fn run_solo(
    base: u32,
    proof_required: bool,
    setup_git: impl FnOnce(&mut GitState),
) -> (Db, RunSummary, u32, Vec<String>) {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    add_task(&db, "t1", TaskState::Ready, Priority::P1);
    let git = RecordingGit::new(tmp.path().join("wt"), base);
    setup_git(&mut git.state.lock().unwrap());
    let gate = Arc::new(RecordingGate {
        git: Arc::clone(&git),
        approved_alone: HashSet::new(),
        red_tasks: HashSet::new(),
        calls: AtomicU32::new(0),
        on_run: Box::new(|_| {}),
    });
    let mut deps = make_deps(
        tmp.path(),
        &db,
        Arc::clone(&git),
        Arc::clone(&gate),
        Arc::new(GatedRunner(None)),
        true,
    );
    deps.settings.repo.proof.required = proof_required;
    let summary = run(&deps, Mode::Once, &hooks_once()).unwrap();
    let events = git.state.lock().unwrap().events.clone();
    (db, summary, gate.calls.load(Ordering::SeqCst), events)
}

fn run_alone_pair(
    auto_merge: bool,
    red_tasks: &[&str],
    on_t1: impl Fn(&Db) + Send + Sync + 'static,
) -> (RunSummary, u32, Vec<String>) {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    add_task(&db, "t0", TaskState::Gating, Priority::P0);
    add_task(&db, "t1", TaskState::Ready, Priority::P1);
    add_task(&db, "t2", TaskState::Ready, Priority::P2);

    let t0_started = Arc::new((Mutex::new(false), Condvar::new()));
    let (t0_flag, db_check) = (Arc::clone(&t0_started), db.clone());
    let git = RecordingGit::new(tmp.path().join("wt"), 0);
    let gate = Arc::new(RecordingGate {
        git: Arc::clone(&git),
        approved_alone: ["t1", "t2"].into_iter().map(String::from).collect(),
        red_tasks: red_tasks.iter().map(|s| (*s).to_string()).collect(),
        calls: AtomicU32::new(0),
        on_run: Box::new(move |id| match id {
            "t0" => {
                *t0_flag.0.lock().unwrap() = true;
                t0_flag.1.notify_all();
                let start = Instant::now();
                while start.elapsed() < TIMEOUT {
                    let g1 = db_check.get_task("t1").unwrap().state == TaskState::Gating;
                    let g2 = db_check.get_task("t2").unwrap().state == TaskState::Gating;
                    if g1 && g2 {
                        std::thread::sleep(Duration::from_millis(30));
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            "t1" => on_t1(&db_check),
            _ => {}
        }),
    });
    let mut deps = make_deps(
        tmp.path(),
        &db,
        Arc::clone(&git),
        Arc::clone(&gate),
        Arc::new(GatedRunner(Some(t0_started))),
        auto_merge,
    );
    deps.settings.max_parallel = 2;
    let summary = run(&deps, Mode::Once, &hooks_once()).unwrap();
    let events = git.state.lock().unwrap().events.clone();
    (summary, gate.calls.load(Ordering::SeqCst), events)
}

#[test]
fn b1_solo_auto_merge_rebases_before_gating_once_and_regates_if_base_moves() {
    let (_db, s1, calls1, ev1) = run_solo(5, false, |_| {});
    assert_eq!((s1.done, s1.gate_runs, calls1), (vec!["t1".into()], 1, 1));
    assert_eq!(
        ev1,
        vec!["rebase:t1@base5", "gate:t1@base5", "merge:t1@base6"]
    );

    // Base moves *after* pre-gate rebase -> merge_with_onto catches onto_sha != covered_base and re-gates.
    let (_db, s2, calls2, _) = run_solo(5, false, |g| {
        g.bump_before_merge.insert("t1".into());
    });
    assert_eq!((s2.done, s2.gate_runs, calls2), (vec!["t1".into()], 2, 2));
}

#[test]
fn b1_solo_rebase_conflict_fails_immediately_with_zero_gate_runs() {
    let (db, summary, calls, _) = run_solo(2, false, |g| {
        g.rebase_conflicts
            .insert("t1".into(), vec!["src/conflict.rs".into()]);
    });
    assert_eq!((calls, summary.gate_runs), (0, 0));
    assert_eq!(summary.requeued, vec!["t1"]);
    let notes = db.get_task("t1").unwrap().context_notes;
    assert!(
        notes.iter().any(|n| n.contains("src/conflict.rs")),
        "{notes:?}"
    );
    let events = db.events_for_task("t1", 50).unwrap();
    assert!(events.iter().any(|e| e.kind == EventKind::MergeResult));
}

#[test]
fn b1_proof_required_skips_pre_gate_rebase() {
    let (_db, summary, calls, events) = run_solo(1, true, |_| {});
    assert_eq!((summary.done, calls), (vec!["t1".into()], 2));
    assert!(
        events.iter().all(|e| !e.starts_with("rebase:")),
        "{events:?}"
    );
}

#[test]
fn b2_auto_merge_alone_jobs_land_sequentially_before_next_rebases_and_gates_once() {
    let (summary, calls, events) = run_alone_pair(true, &[], |_| {});
    assert_eq!((summary.done.len(), calls), (3, 3));
    let (first, second) = if events.iter().any(|e| e == "rebase:t1@base1") {
        ("t1", "t2")
    } else {
        ("t2", "t1")
    };
    let pos = |needle: &str| {
        events
            .iter()
            .position(|e| e == needle)
            .unwrap_or_else(|| panic!("missing {needle} in {events:?}"))
    };
    assert!(
        pos(&format!("merge:{first}@base2")) < pos(&format!("rebase:{second}@base2")),
        "{events:?}"
    );
    assert!(
        pos(&format!("rebase:{second}@base2")) < pos(&format!("gate:{second}@base2")),
        "{events:?}"
    );
}

#[test]
fn b2_pr_mode_alone_jobs_gate_in_parallel_and_stream_answers_immediately() {
    // t2 gates Red: DB state stays Gating until judge_round streams `answer(t2)`
    // to the scheduler AND the scheduler's unblock worker moves t2 to Ready.
    let observed = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&observed);
    let (summary, _, _) = run_alone_pair(false, &["t2"], move |db| {
        let start = Instant::now();
        while start.elapsed() < TIMEOUT {
            if db.get_task("t2").unwrap().state == TaskState::Ready {
                flag.store(true, Ordering::SeqCst);
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    assert_eq!(
        (summary.review.len(), summary.requeued),
        (2, vec!["t2".into()])
    );
    assert!(
        observed.load(Ordering::SeqCst),
        "fast PR-mode alone job t2 must gate in parallel and be answered+unblocked while slow t1 is still gating"
    );
}

#[test]
fn b2b_rerun_failed_test_early_breaks_after_first_pass() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo_dir, worktrees_dir) = (tmp.path().join("repo"), tmp.path().join("worktrees"));
    let wt_dir = worktrees_dir.join("t1");
    fs::create_dir_all(&repo_dir).unwrap();
    fs::create_dir_all(&worktrees_dir).unwrap();
    let run_git = |cwd: &Path, args: &[&str]| {
        let s = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@e.com")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@e.com")
            .status()
            .unwrap();
        assert!(s.success());
    };
    run_git(&repo_dir, &["init", "-b", "main"]);
    fs::write(repo_dir.join("a.txt"), "1\n").unwrap();
    run_git(&repo_dir, &["add", "a.txt"]);
    run_git(&repo_dir, &["commit", "-m", "init"]);
    run_git(
        &repo_dir,
        &[
            "worktree",
            "add",
            "-b",
            "aaf/t1",
            wt_dir.to_str().unwrap(),
            "main",
        ],
    );
    fs::write(wt_dir.join("b.txt"), "2\n").unwrap();
    run_git(&wt_dir, &["add", "b.txt"]);
    run_git(&wt_dir, &["commit", "-m", "change"]);

    let log_file = tmp.path().join("rerun.log");
    let mut cfg = RepoConfig::default();
    cfg.gate.base_ref = "main".into();
    cfg.commands.test_scoped = format!("echo run >> '{}'; exit 0", log_file.display());
    let gate = RealGate {
        data_dir: tmp.path().join("data"),
        cfg,
        clock: Arc::new(SystemClock),
        machine: MachineConfig {
            max_load_percent: 0,
            ..MachineConfig::default()
        },
        progress: Box::new(|_, _| {}),
        worktrees_dir,
    };
    let cmd = CommandResult {
        name: "test_scoped".into(),
        command: "cargo test".into(),
        exit_code: Some(1),
        timed_out: false,
        duration_ms: 50,
        log_tail: "FAILED".into(),
    };
    let receipt = mk_receipt(
        &wt_dir,
        GateStatus::Blocked,
        vec!["command test_scoped failed (exit 1)".into()],
        None,
        vec![cmd],
    );
    let res = gate.rerun_failed_test(&wt_dir, &receipt).unwrap();
    assert_eq!(res, Some(RerunResult { runs: 1, passed: 1 }));
    assert_eq!(fs::read_to_string(&log_file).unwrap().lines().count(), 1);
}
