#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for F1-1, F1-2, and F2-MINOR fixes across C1+C3.

#[path = "git_support.rs"]
mod support;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aaf_domain::account::AccountProfile;
use aaf_domain::config::{LandMode, MachineConfig, RepoConfig};
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::gate::{
    GateMode, GateStatus, HeadLockReport, LockStatus, RECEIPT_SCHEMA_VERSION, Receipt,
    StoreComplianceReport,
};
use aaf_domain::ports::{
    AgentOutcome, AgentRequest, AgentRunner, Clock, EventSink, KvStore, LimitHit,
    META_CONSECUTIVE_ROOT_FAILS, PortError, PortResult, TaskStore,
};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::factory::{
    AccountPool, Deps, GateRunner, GitOps, Grant, Hooks, Integration, Mode, Notifier, RealGit,
    RunSummary, Settings, Store, resolve_base, run,
};
use aaf_engine::git::{
    GhClient, GitError, MergeCommitRef, MergeOutcome, MergeStrategy, PrCheck, PrInfo,
    PrMergeOutcome, PrMergeRequest, PrOutcome, TaskWorktree, merge_pr,
};
use aaf_engine::webhook::WebhookEvent;
use serde_json::json;
use support::{Fixture, commit_file, git};

struct FixedClock(i64);
impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0
    }
}

struct StubDeps {
    allow_agent: bool,
    receipt_head: Option<String>,
}
impl Notifier for StubDeps {
    fn notify(&self, _: &WebhookEvent) -> Result<bool, String> {
        Ok(false)
    }
}
impl AccountPool for StubDeps {
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
impl AgentRunner for StubDeps {
    fn run(&self, _: &AgentRequest) -> PortResult<AgentOutcome> {
        if !self.allow_agent {
            return Err(PortError::Invalid("unexpected agent run".into()));
        }
        Ok(AgentOutcome {
            exit_code: Some(0),
            timed_out: false,
            final_text: "ok".into(),
            tokens_in: 1,
            tokens_out: 1,
            limit_hit: None,
            infra: None,
            log_tail: String::new(),
        })
    }
}
impl GateRunner for StubDeps {
    fn run(&self, tree: &Path) -> PortResult<Receipt> {
        Ok(Receipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            repo_root: tree.display().to_string(),
            tree_fp: "fp".into(),
            head_sha: self.receipt_head.clone(),
            base_sha: None,
            mode: GateMode::Scoped,
            status: GateStatus::Clear,
            reasons: Vec::new(),
            config_hash: "cfg".into(),
            commands: Vec::new(),
            static_findings: Vec::new(),
            suppressions: 0,
            head_lock: self.head_lock(tree)?,
            store_compliance: StoreComplianceReport::not_applicable(),
            created_at: 0,
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

struct StubGh {
    pr: PrInfo,
    bare: PathBuf,
    merge_target: String,
}
impl GhClient for StubGh {
    fn view_pr(&self, _: &Path, _: u64) -> Result<PrInfo, GitError> {
        Ok(self.pr.clone())
    }
    fn pr_checks(&self, _: &Path, _: u64) -> Result<Vec<PrCheck>, GitError> {
        Ok(vec![PrCheck {
            name: "ci".into(),
            state: "COMPLETED".into(),
            bucket: "pass".into(),
        }])
    }
    fn merge_pr(
        &self,
        _: &Path,
        _: u64,
        _: &str,
        _: MergeStrategy,
        _: Option<&str>,
        _: Option<&Path>,
    ) -> Result<(), GitError> {
        let args = ["update-ref", "refs/heads/main", self.merge_target.as_str()];
        git(&self.bare, &args);
        Ok(())
    }
}

macro_rules! fwd_git {
    ($(fn $n:ident(&self $(, $a:ident: $t:ty)*) -> $r:ty;)*) => {
        $(fn $n(&self $(, $a: $t)*) -> $r { self.inner.$n($($a),*) })*
    };
}

struct RealGitWithStubGh {
    inner: RealGit,
    repo: PathBuf,
    data: PathBuf,
    gh: StubGh,
}
impl GitOps for RealGitWithStubGh {
    fwd_git! {
        fn task_worktree(&self, id: &str) -> PortResult<TaskWorktree>;
        fn commit_all(&self, wt: &TaskWorktree, msg: &str) -> PortResult<()>;
        fn has_changes(&self, wt: &TaskWorktree) -> PortResult<bool>;
        fn diff(&self, wt: &TaskWorktree) -> PortResult<String>;
        fn integrate(&self, ids: &[String]) -> PortResult<Integration>;
        fn discard(&self, i: &Integration) -> PortResult<()>;
        fn rebase(&self, wt: &TaskWorktree) -> PortResult<Vec<String>>;
        fn base_sha(&self) -> PortResult<String>;
        fn merge(&self, wt: &TaskWorktree, r: &dyn Fn(&Path) -> Result<bool, String>) -> PortResult<MergeOutcome>;
        fn fetch_base(&self) -> PortResult<()>;
        fn is_merged(&self, id: &str) -> PortResult<bool>;
        fn cleanup(&self, id: &str) -> PortResult<()>;
        fn cleanup_verified_merged(&self, id: &str, head: &str) -> PortResult<()>;
        fn existing_worktree_head(&self, id: &str) -> Result<Option<(TaskWorktree, String)>, GitError>;
    }
    fn open_pr(
        &self,
        _: &TaskWorktree,
        _: &str,
        _: &Receipt,
        _: Option<&str>,
    ) -> PortResult<PrOutcome> {
        Ok(PrOutcome::Opened {
            url: self.gh.pr.url.clone(),
        })
    }
    fn merge_pr(&self, _: &TaskWorktree, _: &str, head: &str) -> PortResult<PrMergeOutcome> {
        let req = PrMergeRequest {
            repo_root: self.repo.clone(),
            data_dir: self.data.clone(),
            pr_number: self.gh.pr.number,
            expect_head: head.to_string(),
            remote: "origin".into(),
            mode: GateMode::Scoped,
            strategy: MergeStrategy::Squash,
            subject: None,
            body_file: None,
            ci_wait_secs: 0,
            require_checks: false,
            lock_timeout: Duration::from_secs(30),
            dry_run: false,
            machine: MachineConfig {
                max_load_percent: 0,
                ..Default::default()
            },
            global_config: None,
        };
        merge_pr(&req, &self.gh, &FixedClock(1_500), &|_| {})
            .map_err(|e| PortError::Io(e.to_string()))
    }
    fn verified_squash_merges(
        &self,
        _: &[(String, String)],
    ) -> Result<HashMap<String, String>, GitError> {
        Ok(HashMap::new())
    }
}

fn real_git(fx: &Fixture) -> RealGit {
    let base = resolve_base(&fx.repo, "auto").unwrap();
    RealGit::new(
        fx.repo.clone(),
        fx.worktrees.clone(),
        fx.locks.clone(),
        base,
    )
}
fn branch_exists(repo: &Path, branch: &str) -> bool {
    git(
        repo,
        &["branch", "--list", "--format=%(refname:short)", branch],
    ) == branch
}
fn snapshots_of(repo: &Path, id: &str) -> String {
    let pref = format!("refs/aaf/snapshots/{id}/");
    git(repo, &["for-each-ref", "--format=%(refname)", &pref])
}

fn setup_repo(fx: &Fixture) -> (PathBuf, Arc<Db>, RealGit) {
    fs::create_dir_all(fx.repo.join(".autofactory")).unwrap();
    fs::write(
        fx.repo.join(".autofactory/config.toml"),
        "[commands]\ntest_scoped = \"true\"\n[gate]\nhead_lock_globs = []\n",
    )
    .unwrap();
    git(&fx.repo, &["add", "-A"]);
    git(&fx.repo, &["commit", "-q", "-m", "gate cfg"]);
    git(&fx.repo, &["push", "-q", "origin", "main"]);
    let data = fx.locks.parent().unwrap().to_path_buf();
    (
        data.clone(),
        Arc::new(Db::open(&data.join("aaf.db")).unwrap()),
        real_git(fx),
    )
}

struct RunSpec<'a> {
    id: &'a str,
    state: TaskState,
    pr: PrInfo,
    retry_head: Option<&'a str>,
    allow_agent: bool,
    receipt_head: Option<String>,
}

fn seed_and_run(
    fx: &Fixture,
    data: &Path,
    db: &Arc<Db>,
    inner: RealGit,
    s: RunSpec<'_>,
) -> RunSummary {
    let spec = ImmutableSpec {
        description: "d".into(),
        acceptance: vec![],
    };
    let mut t = Task::new_root(
        s.id,
        fx.repo.to_string_lossy(),
        s.id,
        spec,
        Priority::P2,
        1_000,
    );
    t.state = s.state;
    db.insert_task(&t).unwrap();
    if let Some(h) = s.retry_head {
        let u = &s.pr.url;
        let retry = json!({"merged": false, "kind": "pr_merge", "code": "ci_pending", "url": u, "head_sha": h});
        for (k, p) in [
            (EventKind::PrOpened, json!({ "url": u })),
            (EventKind::MergeResult, retry),
        ] {
            db.emit(&FactoryEvent::new(k, p.to_string(), 1_000).for_task(s.id))
                .unwrap();
        }
    }
    let merge_target = s.pr.head_ref_oid.clone();
    let gh = StubGh {
        pr: s.pr,
        bare: fx.remote.clone().unwrap(),
        merge_target,
    };
    let git_ops = Arc::new(RealGitWithStubGh {
        inner,
        repo: fx.repo.clone(),
        data: data.to_path_buf(),
        gh,
    });
    let stub = Arc::new(StubDeps {
        allow_agent: s.allow_agent,
        receipt_head: s.receipt_head,
    });
    let mut repo = RepoConfig::default();
    repo.run.land = LandMode::PrMerge;
    let deps = Deps {
        store: Arc::clone(db) as Arc<dyn Store>,
        clock: Arc::new(FixedClock(1_500)),
        runner: Arc::clone(&stub) as Arc<dyn AgentRunner>,
        accounts: Arc::clone(&stub) as Arc<dyn AccountPool>,
        git: git_ops,
        gate: Arc::clone(&stub) as Arc<dyn GateRunner>,
        notifier: stub as Arc<dyn Notifier>,
        settings: Settings {
            repo_root: fx.repo.to_string_lossy().into_owned(),
            repo,
            max_parallel: 1,
            auto_merge: false,
            sandboxed: false,
            hook_required: false,
            agent_path: std::ffi::OsString::new(),
            mobile: false,
            data_dir: data.to_path_buf(),
            home: data.to_path_buf(),
            owner_github_allowlist: Vec::new(),
        },
    };
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };
    run(&deps, Mode::Once, &hooks).unwrap()
}

fn pr_info(num: u64, state: &str, head: &str, id: &str, merge_oid: Option<&str>) -> PrInfo {
    PrInfo {
        number: num,
        state: state.into(),
        head_ref_oid: head.into(),
        head_ref_name: format!("aaf/{id}"),
        base_ref_name: "main".into(),
        mergeable: "MERGEABLE".into(),
        url: format!("https://github.com/org/repo/pull/{num}"),
        merge_commit: merge_oid.map(|o| MergeCommitRef { oid: o.into() }),
        ..PrInfo::default()
    }
}

#[test]
fn f1_1_cleanup_verified_merged_refuses_newer_local_commit_h3_with_and_without_worktree() {
    let fx = Fixture::new();
    let real = real_git(&fx);
    for (id, remove_dir) in [("t_h3_present", false), ("t_h3_missing", true)] {
        let wt = real.task_worktree(id).unwrap();
        let h2 = commit_file(&wt.path, "f.txt", "h2\n", "commit h2");
        let h3 = commit_file(&wt.path, "f.txt", "h3\n", "commit h3");
        let snap = format!("refs/aaf/snapshots/{id}/s1");
        git(&fx.repo, &["update-ref", &snap, &h3]);
        if remove_dir {
            fs::remove_dir_all(&wt.path).unwrap();
        }
        assert!(real.cleanup_verified_merged(id, &h2).is_err());
        assert!(real.cleanup_verified_merged(id, "").is_err());
        assert_eq!(wt.path.is_dir(), !remove_dir);
        assert!(branch_exists(&fx.repo, &format!("aaf/{id}")));
        assert_eq!(fx.head(&fx.repo, &format!("refs/heads/aaf/{id}")), h3);
        assert_eq!(snapshots_of(&fx.repo, id), snap);
    }
}

#[test]
fn f1_2_merge_retry_completes_when_pr_cleanup_db_already_marked_review_done() {
    let fx = Fixture::with_remote();
    let (data, db, real) = setup_repo(&fx);
    db.set_meta(META_CONSECUTIVE_ROOT_FAILS, "2").unwrap();
    let wt = real.task_worktree("t_retry").unwrap();
    let head = commit_file(&wt.path, "feat.txt", "ok\n", "feat");
    git(&wt.path, &["push", "-q", "origin", "aaf/t_retry"]);
    git(
        fx.remote.as_ref().unwrap(),
        &["update-ref", "refs/pull/41/head", &head],
    );
    git(
        &fx.repo,
        &["update-ref", "refs/aaf/snapshots/t_retry/s1", &head],
    );

    let summary = seed_and_run(
        &fx,
        &data,
        &db,
        real,
        RunSpec {
            id: "t_retry",
            state: TaskState::Review,
            pr: pr_info(41, "OPEN", &head, "t_retry", None),
            retry_head: Some(&head),
            allow_agent: false,
            receipt_head: None,
        },
    );
    assert!(summary.done.contains(&"t_retry".to_string()), "{summary:?}");
    assert_eq!(db.get_task("t_retry").unwrap().state, TaskState::Done);
    assert_eq!(
        db.get_meta(META_CONSECUTIVE_ROOT_FAILS).unwrap().as_deref(),
        Some("0")
    );
    assert!(!wt.path.exists() && !branch_exists(&fx.repo, "aaf/t_retry"));
    assert_eq!(snapshots_of(&fx.repo, "t_retry"), "");
}

#[test]
fn f1_1_already_merged_with_unpushed_local_h3_keeps_worktree_in_merge_retry_and_finish_land() {
    for (id, is_retry) in [("t_am_retry", true), ("t_am_land", false)] {
        let fx = Fixture::with_remote();
        let (data, db, real) = setup_repo(&fx);
        let wt = real.task_worktree(id).unwrap();
        let h2 = commit_file(&wt.path, "f.txt", "h2\n", "commit h2");
        git(&wt.path, &["push", "-q", "origin", &wt.branch]);
        git(
            fx.remote.as_ref().unwrap(),
            &["update-ref", "refs/heads/main", &h2],
        );
        let h3 = commit_file(&wt.path, "f.txt", "h3\n", "unpushed h3");
        let snap = format!("refs/aaf/snapshots/{id}/s1");
        git(&fx.repo, &["update-ref", &snap, &h3]);

        let state = if is_retry {
            TaskState::Review
        } else {
            TaskState::Ready
        };
        let summary = seed_and_run(
            &fx,
            &data,
            &db,
            real,
            RunSpec {
                id,
                state,
                pr: pr_info(55, "MERGED", &h2, id, Some(&h2)),
                retry_head: is_retry.then_some(h2.as_str()),
                allow_agent: !is_retry,
                receipt_head: Some(h2.clone()),
            },
        );
        assert!(summary.done.contains(&id.to_string()), "{id}: {summary:?}");
        assert!(wt.path.is_dir() && branch_exists(&fx.repo, &format!("aaf/{id}")));
        assert_eq!(fx.head(&wt.path, "HEAD"), h3);
        assert_eq!(snapshots_of(&fx.repo, id), snap);
    }
}
