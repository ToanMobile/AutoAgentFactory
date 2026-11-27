#![allow(clippy::unwrap_used, clippy::expect_used)]
//! SEC-099: when `executor = "custom"` invokes `claude` via `executor_cmd`
//! (`writer_requires_hook == true`, `StageTarget.hook_required == true`), the
//! worker must enforce Claude's `.claude/settings.json` hook check instead of
//! skipping it because `t.executor == "custom"`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use aaf_domain::account::AccountProfile;
use aaf_domain::blocked::BlockedReason;
use aaf_domain::config::RepoConfig;
use aaf_domain::gate::*;
use aaf_domain::ports::*;
use aaf_domain::task::*;
use aaf_engine::db::Db;
use aaf_engine::factory::adapters::WebhookNotifier;
use aaf_engine::factory::stage_router::writer_requires_hook;
use aaf_engine::factory::*;
use aaf_engine::git::{MergeOutcome, PrOutcome, TaskWorktree};

type Wt = TaskWorktree;
type RegateFn<'a> = &'a dyn Fn(&Path) -> Result<bool, String>;

struct StubInfra(PathBuf, String, AtomicUsize);

impl Clock for StubInfra {
    fn now(&self) -> i64 {
        1_000
    }
}
impl AccountPool for StubInfra {
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
impl AgentRunner for StubInfra {
    fn run(&self, _: &AgentRequest) -> PortResult<AgentOutcome> {
        self.2.fetch_add(1, Ordering::SeqCst);
        Ok(AgentOutcome {
            exit_code: Some(0),
            timed_out: false,
            final_text: "ok".into(),
            tokens_in: 10,
            tokens_out: 5,
            limit_hit: None,
            infra: None,
            log_tail: String::new(),
        })
    }
}
impl GitOps for StubInfra {
    fn task_worktree(&self, id: &str) -> PortResult<Wt> {
        let path = self.0.join(format!("wt-{id}"));
        std::fs::create_dir_all(&path).unwrap();
        Ok(Wt {
            path,
            branch: format!("aaf/{id}"),
        })
    }
    fn commit_all(&self, _: &Wt, _: &str) -> PortResult<()> {
        Ok(())
    }
    fn has_changes(&self, _: &Wt) -> PortResult<bool> {
        Ok(true)
    }
    fn diff(&self, _: &Wt) -> PortResult<String> {
        Ok("diff --git a/src/lib.rs b/src/lib.rs\n+pub fn ok() {}\n".into())
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
    fn rebase(&self, _: &Wt) -> PortResult<Vec<String>> {
        Ok(vec![])
    }
    fn base_sha(&self) -> PortResult<String> {
        Ok("base0".into())
    }
    fn merge(&self, _: &Wt, _: RegateFn<'_>) -> PortResult<MergeOutcome> {
        Ok(MergeOutcome::Merged {
            new_base_sha: "base1".into(),
        })
    }
    fn open_pr(&self, _: &Wt, _: &str, _: &Receipt, _: Option<&str>) -> PortResult<PrOutcome> {
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

type RunOut = (bool, usize, usize, RunSummary, Task);

fn run_single_task(task_id: &str, executor: &str, executor_cmd: &[&str]) -> RunOut {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let repo_root = dir.join("repo").to_string_lossy().into_owned();
    std::fs::create_dir_all(&repo_root).unwrap();
    let db = Db::open(&dir.join("aaf.db")).unwrap();

    let spec = ImmutableSpec {
        description: "Test".into(),
        acceptance: vec!["ok".into()],
    };
    let mut task = Task::new_root(task_id, &repo_root, task_id, spec, Priority::P1, 100);
    task.state = TaskState::Ready;
    db.insert_task(&task).unwrap();

    let mut repo = RepoConfig::default();
    repo.pipeline.plan_debate = false;
    repo.run.executor = executor.into();
    repo.run.executor_cmd = executor_cmd.iter().map(|s| (*s).to_string()).collect();

    let cfg_hash = aaf_engine::gate::config_hash(&repo);
    let infra = Arc::new(StubInfra(dir.clone(), cfg_hash, AtomicUsize::new(0)));
    let cmd_str = executor_cmd.join(" ");
    let target = StageTarget::new_writer(executor, &cmd_str, "", "", infra.clone(), infra.clone());
    let hook_required = target.hook_required;
    let router = StageRouter::new(target);

    let deps = Deps {
        store: Arc::new(db.clone()),
        clock: infra.clone(),
        runner: infra.clone(),
        accounts: infra.clone(),
        git: infra.clone(),
        gate: infra.clone(),
        notifier: Arc::new(WebhookNotifier { url: String::new() }),
        settings: Settings {
            repo_root,
            repo,
            max_parallel: 1,
            auto_merge: false,
            sandboxed: false,
            hook_required,
            agent_path: Default::default(),
            mobile: false,
            data_dir: dir.clone(),
            home: dir,
            owner_github_allowlist: Vec::new(),
        },
    };
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };
    let sum = run_with_router(&deps, None, Some(&router), Mode::Once, &hooks).unwrap();
    let calls = infra.2.load(Ordering::SeqCst);
    let runs = db.runs_for_task(task_id).unwrap().len();
    (
        hook_required,
        calls,
        runs,
        sum,
        db.get_task(task_id).unwrap(),
    )
}

fn assert_blocked_without_hook(task_id: &str, out: &RunOut) {
    let (_, calls, runs, sum, task) = out;
    assert_eq!(sum.blocked, vec![task_id], "SEC-099: sum={sum:?}");
    assert_eq!(*calls, 0, "agent must not run without aaf-hook");
    assert_eq!(*runs, 0);
    assert_eq!(task.state, TaskState::Blocked);
    let Some(BlockedReason::NeedsHuman { why }) = &task.blocked_reason else {
        panic!("unexpected blocked_reason: {:?}", task.blocked_reason);
    };
    assert!(
        why.contains("without aaf-hook") && why.contains(".claude/settings.json"),
        "unexpected blocked_reason: {why}"
    );
}

#[test]
fn custom_claude_executor_missing_hook_settings_blocks_task() {
    assert!(writer_requires_hook("custom", "claude -p {prompt}"));
    let out = run_single_task("t-custom-claude", "custom", &["claude", "-p", "{prompt}"]);
    assert!(out.0);
    assert_blocked_without_hook("t-custom-claude", &out);
}

#[test]
fn custom_non_claude_executor_missing_hook_settings_runs_unblocked() {
    assert!(!writer_requires_hook("custom", "my-agent {prompt}"));
    let (hook_req, calls, _, sum, _) =
        run_single_task("t-custom-other", "custom", &["my-agent", "{prompt}"]);
    assert!(!hook_req);
    assert!(sum.blocked.is_empty(), "summary={sum:?}");
    assert_eq!(sum.review, vec!["t-custom-other"]);
    assert_eq!(calls, 1);
}

#[test]
fn builtin_claude_executor_missing_hook_settings_blocks_task() {
    assert!(writer_requires_hook("claude", ""));
    let out = run_single_task("t-claude", "claude", &[]);
    assert!(out.0);
    assert_blocked_without_hook("t-claude", &out);
}
