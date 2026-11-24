//! Edge-case tests for G1 — Flaky test rerun wiring & precedence.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::blocked::UnblockTier;
use aaf_domain::config::{MachineConfig, RepoConfig};
use aaf_domain::event::EventKind;
use aaf_domain::gate::{
    CommandResult, GateMode, GateStatus, HeadLockReport, LockStatus, RECEIPT_SCHEMA_VERSION,
    Receipt, Severity, StaticFinding, StoreComplianceReport, StoreFinding,
};
use aaf_domain::ports::{
    AgentOutcome, AgentRequest, AgentRunner, Clock, EventSink, LimitHit, PortResult, TaskStore,
};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::factory::adapters::{RealGate, SystemClock};
use aaf_engine::factory::{
    AccountPool, Deps, GateRunner, GitOps, Grant, Hooks, Integration, Mode, Notifier, Settings, run,
};
use aaf_engine::gate::REASON_TREE_CHANGED;
use aaf_engine::git::{MergeOutcome, PrOutcome, TaskWorktree};
use aaf_engine::unblocker::{
    FailureSignal, RerunResult, classify, failure_digest, is_pure_test_failure,
};
use aaf_engine::webhook::WebhookEvent;

fn base_test_receipt() -> Receipt {
    Receipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        repo_root: "/repo".to_string(),
        tree_fp: "fp".to_string(),
        head_sha: None,
        base_sha: None,
        mode: GateMode::Scoped,
        status: GateStatus::Blocked,
        reasons: vec!["command test_scoped failed (exit 1)".to_string()],
        config_hash: "cfg".to_string(),
        commands: vec![CommandResult {
            name: "test_scoped".to_string(),
            command: "cargo test".to_string(),
            exit_code: Some(1),
            timed_out: false,
            duration_ms: 100,
            log_tail: "test a ... FAILED".to_string(),
        }],
        static_findings: Vec::new(),
        suppressions: 0,
        head_lock: HeadLockReport {
            status: LockStatus::Clear,
            lock_fp: String::new(),
            locked_changes: Vec::new(),
            approval: None,
        },
        store_compliance: StoreComplianceReport::not_applicable(),
        created_at: 1,
    }
}

#[test]
fn non_pure_test_gate_blocks_are_rejected_and_keep_gate_failed() {
    let mut lint = base_test_receipt();
    lint.commands[0].name = "lint".to_string();

    let mut build = base_test_receipt();
    build.commands[0].name = "build".to_string();

    let mut timed_out = base_test_receipt();
    timed_out.commands[0].timed_out = true;
    timed_out.commands[0].exit_code = None;

    let mut dirty = base_test_receipt();
    dirty.reasons.push(REASON_TREE_CHANGED.to_string());

    let mut combo = base_test_receipt();
    combo
        .reasons
        .insert(0, "fails only in combination with: t0".to_string());

    let mut static_crit = base_test_receipt();
    static_crit.static_findings.push(StaticFinding {
        rule: "SEC-001".to_string(),
        severity: Severity::Critical,
        file: "src/lib.rs".to_string(),
        line: Some(1),
        message: "secret".to_string(),
        fix: String::new(),
    });

    let mut locked = base_test_receipt();
    locked.head_lock.status = LockStatus::Blocked;

    let mut store_blocked = base_test_receipt();
    store_blocked.store_compliance = StoreComplianceReport::from_findings(
        vec![StoreFinding {
            rule: "STORE-001".to_string(),
            severity: Severity::Critical,
            platform: "android".to_string(),
            guideline: "policy".to_string(),
            file: "AndroidManifest.xml".to_string(),
            line: None,
            message: "blocked".to_string(),
            fix: String::new(),
        }],
        Vec::new(),
    );

    let mut empty_cmds = base_test_receipt();
    empty_cmds.commands.clear();

    for (label, receipt) in [
        ("lint", lint),
        ("build", build),
        ("timed_out", timed_out),
        ("dirty_tree", dirty),
        ("batch_combo", combo),
        ("static_critical", static_crit),
        ("head_lock", locked),
        ("store_compliance", store_blocked),
        ("empty_commands", empty_cmds),
    ] {
        assert!(!is_pure_test_failure(&receipt), "{label} must not be pure");
        let sig = FailureSignal {
            gate: Some(receipt),
            test_rerun: Some(RerunResult { runs: 2, passed: 2 }),
            ..FailureSignal::empty()
        };
        assert_eq!(classify(&sig).kind(), "gate_failed", "{label}");
    }
}

#[test]
fn failure_digest_is_stable_with_or_without_rerun_result() {
    let without = FailureSignal {
        gate: Some(base_test_receipt()),
        test_rerun: None,
        ..FailureSignal::empty()
    };
    let with = FailureSignal {
        gate: Some(base_test_receipt()),
        test_rerun: Some(RerunResult { runs: 2, passed: 2 }),
        ..FailureSignal::empty()
    };
    assert_eq!(failure_digest(&without), failure_digest(&with));
}

// ---- Counting GateRunner fake to verify finish.rs eligibility guards ------

struct FixedClock(AtomicI64);
impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct OkRunner;
impl AgentRunner for OkRunner {
    fn run(&self, _: &AgentRequest) -> PortResult<AgentOutcome> {
        Ok(AgentOutcome {
            exit_code: Some(0),
            timed_out: false,
            final_text: "ok".to_string(),
            tokens_in: 1,
            tokens_out: 1,
            limit_hit: None,
            infra: None,
            log_tail: String::new(),
        })
    }
}

struct Pool(AccountProfile);
impl AccountPool for Pool {
    fn acquire(&self) -> PortResult<Option<Grant>> {
        Ok(Some(Grant::new(Some(self.0.clone()), None)))
    }
    fn report_limit(&self, _: &str, _: &LimitHit) -> PortResult<()> {
        Ok(())
    }
    fn profiles(&self) -> PortResult<Vec<AccountProfile>> {
        Ok(vec![self.0.clone()])
    }
}

struct SilentNotifier;
impl Notifier for SilentNotifier {
    fn notify(&self, _: &WebhookEvent) -> Result<bool, String> {
        Ok(false)
    }
}

struct FakeGit(PathBuf);
impl GitOps for FakeGit {
    fn task_worktree(&self, task_id: &str) -> PortResult<TaskWorktree> {
        Ok(TaskWorktree {
            path: self.0.join(task_id),
            branch: format!("aaf/{task_id}"),
        })
    }
    fn commit_all(&self, _: &TaskWorktree, _: &str) -> PortResult<()> {
        Ok(())
    }
    fn has_changes(&self, _: &TaskWorktree) -> PortResult<bool> {
        Ok(true)
    }
    fn diff(&self, _: &TaskWorktree) -> PortResult<String> {
        Ok("+x\n".to_string())
    }
    fn integrate(&self, task_ids: &[String]) -> PortResult<Integration> {
        Ok(Integration {
            tree: TaskWorktree {
                path: self.0.join("batch"),
                branch: "aaf/batch".to_string(),
            },
            merged: task_ids.to_vec(),
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
        _: Option<&str>,
    ) -> PortResult<PrOutcome> {
        Ok(PrOutcome::Skipped {
            reason: "fake".to_string(),
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

struct CountingGate {
    receipt: Receipt,
    rerun_answer: Option<RerunResult>,
    rerun_calls: AtomicU32,
}
impl GateRunner for CountingGate {
    fn run(&self, _: &Path) -> PortResult<Receipt> {
        Ok(self.receipt.clone())
    }
    fn head_lock(&self, _: &Path) -> PortResult<HeadLockReport> {
        Ok(HeadLockReport {
            status: LockStatus::Clear,
            lock_fp: String::new(),
            locked_changes: Vec::new(),
            approval: None,
        })
    }
    fn rerun_failed_test(&self, _: &Path, _: &Receipt) -> PortResult<Option<RerunResult>> {
        self.rerun_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.rerun_answer)
    }
}

fn run_once_with_gate(
    gate: Arc<CountingGate>,
    preload_t2_note: bool,
) -> (Db, aaf_engine::factory::RunSummary) {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    let mut task = Task::new_root(
        "t1",
        "/fake/repo",
        "edge",
        ImmutableSpec {
            description: "d".to_string(),
            acceptance: vec!["a".to_string()],
        },
        Priority::P1,
        1,
    );
    task.state = TaskState::Ready;
    if preload_t2_note {
        let mut sig = FailureSignal::empty();
        sig.gate = Some(gate.receipt.clone());
        let digest = failure_digest(&sig);
        task.context_notes.push(format!(
            "unblock[{}] digest={digest}: rerun tests",
            UnblockTier::T2Mechanical.as_str()
        ));
    }
    db.insert_task(&task).unwrap();
    let deps = Deps {
        store: Arc::new(db.clone()),
        clock: Arc::new(FixedClock(AtomicI64::new(1_000))),
        runner: Arc::new(OkRunner),
        accounts: Arc::new(Pool(AccountProfile {
            id: "a".to_string(),
            provider: ProviderKind::ClaudeCode,
            config_dir: "/fake/a".to_string(),
            name: "a".to_string(),
        })),
        git: Arc::new(FakeGit(tmp.path().to_path_buf())),
        gate,
        notifier: Arc::new(SilentNotifier),
        settings: Settings {
            repo_root: "/fake/repo".to_string(),
            repo: RepoConfig::default(),
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
    let summary = run(&deps, Mode::Once, &hooks).unwrap();
    (db, summary)
}

#[test]
fn non_test_failure_does_not_call_rerun() {
    let mut lint_red = base_test_receipt();
    lint_red.commands[0].name = "lint".to_string();
    let gate = Arc::new(CountingGate {
        receipt: lint_red,
        rerun_answer: Some(RerunResult { runs: 2, passed: 2 }),
        rerun_calls: AtomicU32::new(0),
    });
    let (_db, summary) = run_once_with_gate(Arc::clone(&gate), false);
    assert_eq!(gate.rerun_calls.load(Ordering::SeqCst), 0);
    assert_eq!(summary.requeued, vec!["t1"]);
}

#[test]
fn timeout_and_batch_combo_do_not_call_rerun() {
    let mut timed_out = base_test_receipt();
    timed_out.commands[0].timed_out = true;
    timed_out.commands[0].exit_code = None;
    let g1 = Arc::new(CountingGate {
        receipt: timed_out,
        rerun_answer: Some(RerunResult { runs: 2, passed: 2 }),
        rerun_calls: AtomicU32::new(0),
    });
    let _ = run_once_with_gate(Arc::clone(&g1), false);
    assert_eq!(g1.rerun_calls.load(Ordering::SeqCst), 0);

    let mut combo = base_test_receipt();
    combo
        .reasons
        .insert(0, "fails only in combination with: t0".to_string());
    let g2 = Arc::new(CountingGate {
        receipt: combo,
        rerun_answer: Some(RerunResult { runs: 2, passed: 2 }),
        rerun_calls: AtomicU32::new(0),
    });
    let _ = run_once_with_gate(Arc::clone(&g2), false);
    assert_eq!(g2.rerun_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn t2_already_tried_skips_redundant_rerun_and_climbs_to_t3() {
    let gate = Arc::new(CountingGate {
        receipt: base_test_receipt(),
        rerun_answer: Some(RerunResult { runs: 2, passed: 2 }),
        rerun_calls: AtomicU32::new(0),
    });
    let (db, summary) = run_once_with_gate(Arc::clone(&gate), true);
    assert_eq!(gate.rerun_calls.load(Ordering::SeqCst), 0);
    assert_eq!(summary.requeued, vec!["t1"]);
    let events = db.events_for_task("t1", 50).unwrap();
    let step = events
        .iter()
        .find(|e| e.kind == EventKind::UnblockStep)
        .unwrap();
    let payload: serde_json::Value = serde_json::from_str(&step.payload_json).unwrap();
    assert_eq!(payload["tier"].as_str(), Some("T3"));
}

#[test]
fn deterministic_failure_zero_of_two_stays_gate_failed() {
    let gate = Arc::new(CountingGate {
        receipt: base_test_receipt(),
        rerun_answer: Some(RerunResult { runs: 2, passed: 0 }),
        rerun_calls: AtomicU32::new(0),
    });
    let (db, summary) = run_once_with_gate(Arc::clone(&gate), false);
    assert_eq!(gate.rerun_calls.load(Ordering::SeqCst), 1);
    assert_eq!(summary.requeued, vec!["t1"]);
    let events = db.events_for_task("t1", 50).unwrap();
    let step = events
        .iter()
        .find(|e| e.kind == EventKind::UnblockStep)
        .unwrap();
    let payload: serde_json::Value = serde_json::from_str(&step.payload_json).unwrap();
    assert_eq!(payload["reason"]["kind"].as_str(), Some("gate_failed"));
    assert_eq!(payload["tier"].as_str(), Some("T3"));
}

/// Runs `RealGate::rerun_failed_test` with `test_scoped = cmd` on a task
/// worktree (one commit ahead of `main`) that holds `sneaky.txt`, a file
/// hidden by `.git/info/exclude`. Returns the answer and the worktree.
fn real_gate_rerun(tmp: &Path, cmd: String) -> (Option<RerunResult>, PathBuf) {
    let repo_dir = tmp.join("repo");
    let worktrees_dir = tmp.join("worktrees");
    let wt_dir = worktrees_dir.join("t1");
    fs::create_dir_all(&repo_dir).unwrap();
    fs::create_dir_all(&worktrees_dir).unwrap();
    let git = |cwd: &Path, args: &[&str]| {
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
    git(&repo_dir, &["init", "-b", "main"]);
    fs::write(repo_dir.join("a.txt"), "1\n").unwrap();
    git(&repo_dir, &["add", "a.txt"]);
    git(&repo_dir, &["commit", "-m", "init"]);
    git(
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
    git(&wt_dir, &["add", "b.txt"]);
    git(&wt_dir, &["commit", "-m", "change"]);

    // Plant a hidden file ignored via .git/info/exclude; rerun must remove it.
    fs::write(repo_dir.join(".git/info/exclude"), "sneaky.txt\n").unwrap();
    fs::write(wt_dir.join("sneaky.txt"), "hidden\n").unwrap();

    let mut cfg = RepoConfig::default();
    cfg.gate.base_ref = "main".to_string();
    cfg.commands.test_scoped = cmd;

    let gate = RealGate {
        data_dir: tmp.join("data"),
        cfg,
        clock: Arc::new(SystemClock),
        machine: MachineConfig {
            max_load_percent: 0,
            ..MachineConfig::default()
        },
        progress: Box::new(|_, _| {}),
        worktrees_dir,
    };

    // Put a bogus command inside receipt to confirm RealGate uses cfg.commands.
    let mut receipt = base_test_receipt();
    receipt.commands[0].command = "exit 99".to_string();

    (gate.rerun_failed_test(&wt_dir, &receipt).unwrap(), wt_dir)
}

#[test]
fn real_gate_rerun_sets_diff_env_removes_hidden_and_uses_cfg_command() {
    let tmp = tempfile::tempdir().unwrap();
    // Fails on rerun 1, passes on rerun 2; also verifies diff-env file exists
    // and sneaky.txt was removed.
    let marker = tmp.path().join("count.flag");
    let cmd = format!(
        "test -f \"$AAF_CHANGED_FILES_FILE\" && test ! -f sneaky.txt && if [ ! -f '{}' ]; then touch '{}'; exit 1; fi; exit 0",
        marker.display(),
        marker.display()
    );

    let (res, wt_dir) = real_gate_rerun(tmp.path(), cmd);
    assert_eq!(res, Some(RerunResult { runs: 2, passed: 1 }));
    assert!(!wt_dir.join("sneaky.txt").exists());
}

/// One passing rerun is all the classifier needs (`passed > 0`): a second
/// run of a slow suite would only cost time.
#[test]
fn real_gate_rerun_stops_at_the_first_rerun_that_passes() {
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("runs.log");
    let cmd = format!("echo run >> '{}'; exit 0", log.display());

    let (res, _) = real_gate_rerun(tmp.path(), cmd);

    let executions = fs::read_to_string(&log).unwrap().lines().count();
    assert_eq!(executions, 1, "the test command ran {executions} times");
    assert_eq!(res, Some(RerunResult { runs: 1, passed: 1 }));
}
