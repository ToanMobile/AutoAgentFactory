//! Oracle test for G1 — Wire flaky test rerun.
//!
//! **Phase 0 (RED)**: Frozen after creation on base `92367ec`.
//! Compiles on `92367ec` using existing public APIs, FAILS at assertions on
//! `92367ec`, and PASSES after G1 is fixed.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::config::{MachineConfig, RepoConfig};
use aaf_domain::event::EventKind;
use aaf_domain::gate::{
    CommandResult, GateMode, GateStatus, HeadLockReport, LockStatus, RECEIPT_SCHEMA_VERSION,
    Receipt, StoreComplianceReport,
};
use aaf_domain::ports::{
    AgentOutcome, AgentRequest, AgentRunner, Clock, EventSink, LimitHit, PortResult, TaskStore,
};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::factory::adapters::{RealGate, SystemClock};
use aaf_engine::factory::{
    AccountPool, Deps, GitOps, Grant, Hooks, Integration, Mode, Notifier, Settings, run,
};
use aaf_engine::git::{MergeOutcome, PrOutcome, TaskWorktree};
use aaf_engine::unblocker::{FailureSignal, RerunResult, classify};
use aaf_engine::webhook::WebhookEvent;

fn pure_test_blocked_receipt() -> Receipt {
    Receipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        repo_root: "/repo".to_string(),
        tree_fp: "fp-oracle".to_string(),
        head_sha: None,
        base_sha: None,
        mode: GateMode::Scoped,
        status: GateStatus::Blocked,
        reasons: vec!["command test_scoped failed (exit 101)".to_string()],
        config_hash: "cfg".to_string(),
        commands: vec![CommandResult {
            name: "test_scoped".to_string(),
            command: "cargo test".to_string(),
            exit_code: Some(101),
            timed_out: false,
            duration_ms: 500,
            log_tail: "test flaky ... FAILED".to_string(),
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

/// Classifier oracle: a pure test-command gate block accompanied by a passing
/// rerun on the unchanged tree must classify as `FlakyTest`, not `GateFailed`.
#[test]
fn classifier_promotes_pure_test_gate_failure_with_passing_rerun_to_flaky_test() {
    let signal = FailureSignal {
        gate: Some(pure_test_blocked_receipt()),
        test_rerun: Some(RerunResult { runs: 2, passed: 2 }),
        ..FailureSignal::empty()
    };
    assert_eq!(
        classify(&signal).kind(),
        "flaky_test",
        "pure test gate block with passing rerun must classify as FlakyTest"
    );
}

// ---- End-to-end factory pipeline oracle -----------------------------------

struct FixedClock(AtomicI64);
impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct OkRunner;
impl AgentRunner for OkRunner {
    fn run(&self, _req: &AgentRequest) -> PortResult<AgentOutcome> {
        Ok(AgentOutcome {
            exit_code: Some(0),
            timed_out: false,
            final_text: "done".to_string(),
            tokens_in: 10,
            tokens_out: 10,
            limit_hit: None,
            infra: None,
            log_tail: String::new(),
        })
    }
}

struct OneAccountPool(AccountProfile);
impl AccountPool for OneAccountPool {
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

struct SingleWorktreeGit {
    worktree: PathBuf,
}
impl GitOps for SingleWorktreeGit {
    fn task_worktree(&self, task_id: &str) -> PortResult<TaskWorktree> {
        Ok(TaskWorktree {
            path: self.worktree.clone(),
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
        Ok("+change\n".to_string())
    }
    fn integrate(&self, task_ids: &[String]) -> PortResult<Integration> {
        Ok(Integration {
            tree: TaskWorktree {
                path: self.worktree.clone(),
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
            reason: "oracle".to_string(),
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

fn git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "Oracle")
        .env("GIT_AUTHOR_EMAIL", "oracle@example.com")
        .env("GIT_COMMITTER_NAME", "Oracle")
        .env("GIT_COMMITTER_EMAIL", "oracle@example.com")
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed");
}

/// End-to-end oracle: `test_scoped` fails on run 1 and passes on runs 2+
/// without any code change. The factory must rerun the failed test, classify
/// `FlakyTest`, execute T2 `RerunTests` (re-gate), and land the task in
/// `Review` without burning another agent run.
#[test]
fn factory_reruns_flaky_test_classifies_flaky_and_regates_to_review() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    let worktrees_dir = tmp.path().join("worktrees");
    let wt_dir = worktrees_dir.join("t1");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&repo_dir).unwrap();
    fs::create_dir_all(&worktrees_dir).unwrap();
    fs::create_dir_all(&data_dir).unwrap();

    git(&repo_dir, &["init", "-b", "main"]);
    fs::write(repo_dir.join("README.md"), "hello\n").unwrap();
    git(&repo_dir, &["add", "README.md"]);
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
    fs::write(wt_dir.join("feature.txt"), "ok\n").unwrap();
    git(&wt_dir, &["add", "feature.txt"]);
    git(&wt_dir, &["commit", "-m", "task change"]);

    // Fails on run 1, passes on runs 2+. Marker lives outside the git tree so
    // the worktree fingerprint stays untouched.
    let marker = tmp.path().join("flaky.marker");
    let flaky_cmd = format!(
        "if [ ! -f '{}' ]; then touch '{}'; echo 'test flaky ... FAILED'; exit 1; fi; echo 'test flaky ... ok'",
        marker.display(),
        marker.display()
    );

    let mut repo_cfg = RepoConfig::default();
    repo_cfg.gate.base_ref = "main".to_string();
    repo_cfg.commands.test_scoped = flaky_cmd;

    let gate = Arc::new(RealGate {
        data_dir: data_dir.clone(),
        cfg: repo_cfg.clone(),
        clock: Arc::new(SystemClock),
        machine: MachineConfig {
            max_load_percent: 0,
            ..MachineConfig::default()
        },
        progress: Box::new(|_, _| {}),
        worktrees_dir: worktrees_dir.clone(),
    });

    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    let mut task = Task::new_root(
        "t1",
        repo_dir.to_str().unwrap(),
        "Flaky test task",
        ImmutableSpec {
            description: "Implement feature".to_string(),
            acceptance: vec!["tests pass".to_string()],
        },
        Priority::P1,
        1,
    );
    task.state = TaskState::Ready;
    db.insert_task(&task).unwrap();

    let deps = Deps {
        store: Arc::new(db.clone()),
        clock: Arc::new(FixedClock(AtomicI64::new(1_000))),
        runner: Arc::new(OkRunner),
        accounts: Arc::new(OneAccountPool(AccountProfile {
            id: "acc1".to_string(),
            provider: ProviderKind::ClaudeCode,
            config_dir: tmp.path().join("acc1").to_string_lossy().into_owned(),
            name: "acc1".to_string(),
        })),
        git: Arc::new(SingleWorktreeGit { worktree: wt_dir }),
        gate,
        notifier: Arc::new(SilentNotifier),
        settings: Settings {
            repo_root: repo_dir.to_string_lossy().into_owned(),
            repo: repo_cfg,
            max_parallel: 1,
            auto_merge: false,
            sandboxed: false,
            hook_required: false,
            agent_path: Default::default(),
            mobile: false,
            data_dir,
            home: tmp.path().to_path_buf(),
        },
    };

    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };
    let summary = run(&deps, Mode::Once, &hooks).unwrap();

    let events = db.events_for_task("t1", 100).unwrap();
    let unblock_step = events
        .iter()
        .find(|e| e.kind == EventKind::UnblockStep)
        .expect("expected an UnblockStep event after initial red gate");
    let payload: serde_json::Value = serde_json::from_str(&unblock_step.payload_json).unwrap();

    assert_eq!(
        payload["reason"]["kind"].as_str(),
        Some("flaky_test"),
        "expected FlakyTest classification, got payload: {payload}"
    );
    assert_eq!(
        payload["action"]["action"].as_str(),
        Some("rerun_tests"),
        "expected T2 RerunTests action, got payload: {payload}"
    );
    assert_eq!(summary.agent_runs, 1, "flaky recovery must not rerun agent");
    assert_eq!(
        summary.review,
        vec!["t1"],
        "re-gate after flaky classification must pass into Review"
    );
}
