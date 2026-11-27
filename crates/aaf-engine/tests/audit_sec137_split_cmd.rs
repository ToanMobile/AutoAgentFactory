#![allow(clippy::unwrap_used, clippy::expect_used, clippy::unimplemented)]
//! SEC-137 (rework 1, from counter-review): `[run].executor_cmd` is a Vec;
//! joining it with spaces and re-splitting it must not hide that Claude runs
//! (an argument with an apostrophe, or a space in the path to `claude`).
//! Only `.codex/hooks.json` is registered, so the task must be blocked.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{fs, process::Command, sync::Arc};

use aaf_domain::{account::AccountProfile, config::RepoConfig, gate::*, ports::*, task::*};
use aaf_engine::factory::{adapters::WebhookNotifier, *};
use aaf_engine::git::{MergeOutcome, PrOutcome, TaskWorktree};
use aaf_engine::{db::Db, hook_settings::multi_host};

type Wt = TaskWorktree;
type RegateFn<'a> = &'a dyn Fn(&Path) -> Result<bool, String>;
type RunOut = (usize, usize, RunSummary, Task);

fn git(dir: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    for k in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"] {
        cmd.env_remove(k);
    }
    let id = [
        ("GIT_AUTHOR_NAME", "a"),
        ("GIT_AUTHOR_EMAIL", "a@b"),
        ("GIT_COMMITTER_NAME", "a"),
        ("GIT_COMMITTER_EMAIL", "a@b"),
    ];
    let out = cmd.envs(id).output().unwrap();
    assert!(out.status.success(), "{args:?}");
    String::from_utf8_lossy(&out.stdout).trim().into()
}

struct StubInfra(PathBuf, String, String, AtomicUsize);

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
        self.3.fetch_add(1, Ordering::SeqCst);
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
        Ok(Wt {
            path: self.0.clone(),
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
        Ok("diff --git a/a b/a\n+x\n".into())
    }
    fn integrate(&self, _: &[String]) -> PortResult<Integration> {
        unimplemented!()
    }
    fn discard(&self, _: &Integration) -> PortResult<()> {
        Ok(())
    }
    fn rebase(&self, _: &Wt) -> PortResult<Vec<String>> {
        Ok(vec![])
    }
    fn base_sha(&self) -> PortResult<String> {
        Ok(self.1.clone())
    }
    fn merge(&self, _: &Wt, _: RegateFn<'_>) -> PortResult<MergeOutcome> {
        unimplemented!()
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
            head_sha: Some(self.1.clone()),
            base_sha: Some(self.1.clone()),
            mode: GateMode::Scoped,
            status: GateStatus::Clear,
            reasons: vec![],
            config_hash: self.2.clone(),
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

fn run_with_codex_hook(task_id: &str, executor: &str, executor_cmd: &[&str]) -> RunOut {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    let repo_dir = root.join("repo");
    let wt = root.join(format!("wt-{task_id}"));
    fs::create_dir_all(&repo_dir).unwrap();
    fs::create_dir_all(&wt).unwrap();

    let hook = root.join("aaf-hook");
    fs::write(&hook, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();

    git(&wt, &["init", "-q", "-b", "main"]);
    multi_host::register_codex(&wt, hook.to_str().unwrap()).unwrap();
    git(&wt, &["add", ".codex/hooks.json"]);
    git(&wt, &["commit", "-qm", "h", "--no-gpg-sign", "--no-verify"]);
    let base = git(&wt, &["rev-parse", "HEAD"]);

    let repo_root = repo_dir.to_string_lossy().into_owned();
    let db = Db::open(&root.join("aaf.db")).unwrap();
    let spec = ImmutableSpec {
        description: "t".into(),
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
    let infra = Arc::new(StubInfra(wt, base, cfg_hash, AtomicUsize::new(0)));
    let cmd_str = executor_cmd.join(" ");
    let target = StageTarget::new_writer(executor, &cmd_str, "", "", infra.clone(), infra.clone());
    let mut target = target;
    // aaf-cli stage_wiring: def.hook_required |= run_executor_is_claude(run) (arg0 basename of the Vec)
    target.hook_required = true;
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
            data_dir: root.clone(),
            home: root,
            owner_github_allowlist: Vec::new(),
        },
    };
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };
    let sum = run_with_router(&deps, None, Some(&router), Mode::Once, &hooks).unwrap();
    let calls = infra.3.load(Ordering::SeqCst);
    let runs = db.runs_for_task(task_id).unwrap().len();
    let task = db.get_task(task_id).unwrap();
    (calls, runs, sum, task)
}

fn assert_blocked(id: &str, cmd: &[&str]) {
    let (calls, runs, sum, task) = run_with_codex_hook(id, "codex", cmd);
    eprintln!(
        "{id}: calls={calls} runs={runs} sum={sum:?} state={:?} reason={:?}",
        task.state, task.blocked_reason
    );
    assert_eq!(
        calls, 0,
        "MC: Claude was spawned with only .codex/hooks.json checked: {cmd:?}"
    );
}

#[test]
fn mc_apostrophe_arg_breaks_claude_detection() {
    assert_blocked(
        "t-apos",
        &[
            "claude",
            "--append-system-prompt",
            "don't push",
            "-p",
            "{prompt}",
        ],
    );
}

#[test]
fn mc_space_in_claude_path_breaks_claude_detection() {
    assert_blocked("t-space", &["/opt/my tools/claude", "-p", "{prompt}"]);
}
