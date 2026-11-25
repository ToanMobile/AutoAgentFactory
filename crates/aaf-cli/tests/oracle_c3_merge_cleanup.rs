#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Oracle tests for Gói C3: `aaf merge` local/remote cleanup, `isCrossRepository`,
//! `AlreadyMerged` fail-closed guards, and `caller_cwd`/dirty/active-task protection.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use aaf_domain::event::EventKind;
use aaf_domain::ports::{EventSink, TaskStore};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::git::worktree;
use serde_json::Value;
use tempfile::TempDir;

const AAF_BIN: &str = env!("CARGO_BIN_EXE_aaf");

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn ref_exists(repo: &Path, refname: &str) -> bool {
    Command::new("git")
        .args(["show-ref", "--verify", "--quiet", refname])
        .current_dir(repo)
        .status()
        .unwrap()
        .success()
}

struct Fixture {
    _tmp: TempDir,
    root: PathBuf,
    bare: PathBuf,
    repo: PathBuf,
    data: PathBuf,
    config: PathBuf,
    worktrees: PathBuf,
    bin_dir: PathBuf,
    gh_log: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let bare = root.join("origin.git");
        let repo = root.join("repo");
        let data = root.join("data");
        let config = root.join("config");
        let worktrees = root.join("worktrees");
        let bin_dir = root.join("bin");
        let gh_log = root.join("gh.log");
        for dir in [&bare, &repo, &data, &config, &worktrees, &bin_dir] {
            fs::create_dir_all(dir).unwrap();
        }
        // Disable CPU load throttling in global config so tests never stall on busy hosts.
        fs::write(
            config.join("config.toml"),
            "[machine]\nmax_load_percent = 0\n",
        )
        .unwrap();

        git(&bare, &["init", "--bare", "-b", "main"]);
        git(&repo, &["init", "-b", "main"]);
        git(&repo, &["remote", "add", "origin", bare.to_str().unwrap()]);
        fs::create_dir_all(repo.join(".autofactory")).unwrap();
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(
            repo.join(".autofactory/config.toml"),
            "[commands]\ntest_scoped = \"true\"\n[gate]\nhead_lock_globs = []\n",
        )
        .unwrap();
        fs::write(repo.join("src/lib.rs"), "pub fn v() -> i32 { 1 }\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-m", "initial"]);
        git(&repo, &["push", "-u", "origin", "main"]);

        Self {
            _tmp: tmp,
            root,
            bare,
            repo,
            data,
            config,
            worktrees,
            bin_dir,
            gh_log,
        }
    }

    fn create_task_pr(
        &self,
        task_id: &str,
        pr_number: u64,
        db_state: Option<TaskState>,
    ) -> (PathBuf, String) {
        let wt = worktree::create(&self.repo, &self.worktrees, task_id, "main").unwrap();
        fs::write(
            wt.path.join("src/lib.rs"),
            format!("pub fn v() -> i32 {{ {pr_number} }}\n"),
        )
        .unwrap();
        git(&wt.path, &["add", "-A"]);
        git(&wt.path, &["commit", "-m", &format!("feat {task_id}")]);
        let head = git(&wt.path, &["rev-parse", "HEAD"]);
        git(&wt.path, &["push", "-u", "origin", &wt.branch]);
        git(
            &self.bare,
            &["update-ref", &format!("refs/pull/{pr_number}/head"), &head],
        );
        git(
            &self.repo,
            &[
                "update-ref",
                &format!("refs/aaf/snapshots/{task_id}/turn-1"),
                &head,
            ],
        );
        if let Some(state) = db_state {
            let db = Db::open(&self.data.join("aaf.db")).unwrap();
            let mut task = Task::new_root(
                task_id,
                self.repo.to_string_lossy().into_owned(),
                format!("Task {task_id}"),
                ImmutableSpec {
                    description: "test".into(),
                    acceptance: vec![],
                },
                Priority::P2,
                100,
            );
            task.state = state;
            db.insert_task(&task).unwrap();
        }
        (wt.path, head)
    }

    fn write_gh_stub(
        &self,
        pr_number: u64,
        state: &str,
        head: &str,
        branch: &str,
        is_cross_repo: bool,
        merge_commit_oid: Option<&str>,
    ) {
        let merge_commit_json = match merge_commit_oid {
            Some(oid) => format!(r#"{{"oid":"{oid}"}}"#),
            None => "null".to_string(),
        };
        let view_json = format!(
            r#"{{"number":{pr_number},"state":"{state}","headRefOid":"{head}","headRefName":"{branch}","baseRefName":"main","mergeable":"MERGEABLE","isDraft":false,"title":"PR #{pr_number}","url":"https://github.com/org/repo/pull/{pr_number}","isCrossRepository":{is_cross_repo},"mergeCommit":{merge_commit_json}}}"#
        );
        let script = format!(
            r#"#!/bin/sh
echo "gh $*" >> '{log}'
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  printf '%s\n' '{view_json}'
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "checks" ]; then
  printf '[]\n'
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "merge" ]; then
  git --git-dir='{bare}' update-ref refs/heads/main '{head}'
  exit 0
fi
echo "unexpected gh args: $*" >&2
exit 1
"#,
            log = self.gh_log.display(),
            bare = self.bare.display(),
        );
        let gh_path = self.bin_dir.join("gh");
        fs::write(&gh_path, script).unwrap();
        fs::set_permissions(&gh_path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn run_aaf(&self, cwd: &Path, args: &[&str]) -> (i32, Value, String) {
        let sys_path = std::env::var("PATH").unwrap_or_default();
        let path_env = format!("{}:{sys_path}", self.bin_dir.display());
        let out = Command::new(AAF_BIN)
            .args(args)
            .current_dir(cwd)
            .env("PATH", path_env)
            .env("HOME", self.root.join("home"))
            .env("AAF_DATA_DIR", &self.data)
            .env("AAF_CONFIG_DIR", &self.config)
            .env("AAF_WORKTREE_DIR", &self.worktrees)
            .env_remove("AAF_RUN_ID")
            .output()
            .unwrap();
        let code = out.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let json: Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
            panic!("invalid JSON from aaf {args:?} (exit {code}): {e}\nstdout:\n{stdout}\nstderr:\n{stderr}")
        });
        (code, json, stderr)
    }
}

#[test]
fn c3_merge_cleans_worktree_branch_remote_snapshots_and_marks_review_done() {
    let fx = Fixture::new();
    let (wt, head) = fx.create_task_pr("t1", 7, Some(TaskState::Review));
    fx.write_gh_stub(7, "OPEN", &head, "aaf/t1", false, None);

    let (code, json, stderr) =
        fx.run_aaf(&fx.repo, &["merge", "7", "--expect-head", &head, "--json"]);
    assert_eq!(code, 0, "json={json} stderr={stderr}");
    assert_eq!(json["status"], "MERGED");

    let log = fs::read_to_string(&fx.gh_log).unwrap();
    assert!(log.contains("pr merge 7"), "gh log:\n{log}");
    assert!(
        !log.contains("--delete-branch"),
        "gh pr merge must NOT pass --delete-branch:\n{log}"
    );

    assert!(
        !wt.exists(),
        "linked worktree must be removed: {}",
        wt.display()
    );
    assert!(
        !ref_exists(&fx.repo, "refs/heads/aaf/t1"),
        "local branch aaf/t1 must be deleted"
    );
    assert!(
        !ref_exists(&fx.bare, "refs/heads/aaf/t1"),
        "remote branch aaf/t1 must be deleted"
    );
    assert!(
        !ref_exists(&fx.repo, "refs/aaf/snapshots/t1/turn-1"),
        "snapshot ref must be deleted"
    );

    let db = Db::open(&fx.data.join("aaf.db")).unwrap();
    assert_eq!(db.get_task("t1").unwrap().state, TaskState::Done);
    let events = db.events_for_task("t1", 20).unwrap();
    assert!(
        events.iter().any(|e| e.kind == EventKind::TaskStateChanged),
        "expected TaskStateChanged event"
    );
    assert!(
        events.iter().any(|e| e.kind == EventKind::MergeResult),
        "expected MergeResult event"
    );
}

#[test]
fn c3_merge_preserves_worktree_branch_snapshots_for_caller_cwd_and_dirty_tree() {
    // Case A: invoked from inside `wt/subdir` (`caller_cwd`).
    let fx_cwd = Fixture::new();
    let (wt_cwd, head_cwd) = fx_cwd.create_task_pr("t_cwd", 8, Some(TaskState::Review));
    let subdir = wt_cwd.join("src");
    fx_cwd.write_gh_stub(8, "OPEN", &head_cwd, "aaf/t_cwd", false, None);

    let (code, json, stderr) = fx_cwd.run_aaf(
        &subdir,
        &["merge", "8", "--expect-head", &head_cwd, "--json"],
    );
    assert_eq!(code, 0, "json={json} stderr={stderr}");
    assert_eq!(json["status"], "MERGED");
    assert!(wt_cwd.exists(), "worktree containing caller_cwd must stay");
    assert!(
        ref_exists(&fx_cwd.repo, "refs/heads/aaf/t_cwd"),
        "local branch must stay when worktree skipped"
    );
    assert!(
        ref_exists(&fx_cwd.repo, "refs/aaf/snapshots/t_cwd/turn-1"),
        "snapshot ref must stay when worktree skipped"
    );
    let note = json["local_sync_note"].as_str().unwrap_or_default();
    assert!(
        note.contains("warning:"),
        "expected warning when skipping cwd worktree, got: {json}"
    );
    let db_cwd = Db::open(&fx_cwd.data.join("aaf.db")).unwrap();
    assert_eq!(db_cwd.get_task("t_cwd").unwrap().state, TaskState::Done);

    // Case B: dirty untracked file inside linked worktree.
    let fx_dirty = Fixture::new();
    let (wt_dirty, head_dirty) = fx_dirty.create_task_pr("t_dirty", 9, Some(TaskState::Review));
    fs::write(wt_dirty.join("untracked.txt"), "keep me\n").unwrap();
    fx_dirty.write_gh_stub(9, "OPEN", &head_dirty, "aaf/t_dirty", false, None);

    let (code, json, stderr) = fx_dirty.run_aaf(
        &fx_dirty.repo,
        &["merge", "9", "--expect-head", &head_dirty, "--json"],
    );
    assert_eq!(code, 0, "json={json} stderr={stderr}");
    assert_eq!(json["status"], "MERGED");
    assert!(wt_dirty.exists(), "dirty worktree must stay");
    assert!(
        ref_exists(&fx_dirty.repo, "refs/heads/aaf/t_dirty"),
        "local branch must stay for dirty worktree"
    );
    assert!(
        ref_exists(&fx_dirty.repo, "refs/aaf/snapshots/t_dirty/turn-1"),
        "snapshot ref must stay for dirty worktree"
    );
    let note = json["local_sync_note"].as_str().unwrap_or_default();
    assert!(
        note.contains("warning:"),
        "expected warning for dirty worktree, got: {json}"
    );
}

#[test]
fn c3_merge_skips_local_remote_and_db_for_cross_repository_pr() {
    let fx = Fixture::new();
    let (wt, head) = fx.create_task_pr("t_fork", 10, Some(TaskState::Review));
    fx.write_gh_stub(10, "OPEN", &head, "aaf/t_fork", true, None);

    let (code, json, stderr) =
        fx.run_aaf(&fx.repo, &["merge", "10", "--expect-head", &head, "--json"]);
    assert_eq!(code, 0, "json={json} stderr={stderr}");
    assert_eq!(json["status"], "MERGED");

    assert!(wt.exists(), "cross-repo PR must not remove local worktree");
    assert!(
        ref_exists(&fx.repo, "refs/heads/aaf/t_fork"),
        "cross-repo PR must not delete local branch"
    );
    assert!(
        ref_exists(&fx.bare, "refs/heads/aaf/t_fork"),
        "cross-repo PR must not delete remote branch"
    );
    assert!(
        ref_exists(&fx.repo, "refs/aaf/snapshots/t_fork/turn-1"),
        "cross-repo PR must not delete snapshot ref"
    );
    let db = Db::open(&fx.data.join("aaf.db")).unwrap();
    assert_eq!(
        db.get_task("t_fork").unwrap().state,
        TaskState::Review,
        "cross-repo PR must not touch aaf.db"
    );
}

#[test]
fn c3_already_merged_fails_closed_on_dry_run_and_head_mismatch_then_cleans_verified() {
    let fx = Fixture::new();
    let (wt, head) = fx.create_task_pr("t_am", 11, Some(TaskState::Review));
    // Simulate PR already merged on origin/main with mergeCommit == head.
    git(&fx.bare, &["update-ref", "refs/heads/main", &head]);
    fx.write_gh_stub(11, "MERGED", &head, "aaf/t_am", false, Some(&head));

    // 1. --dry-run must make zero disk/DB changes.
    let (code, json, stderr) = fx.run_aaf(
        &fx.repo,
        &["merge", "11", "--expect-head", &head, "--dry-run", "--json"],
    );
    assert_eq!(code, 0, "json={json} stderr={stderr}");
    assert_eq!(json["status"], "ALREADY_MERGED");
    assert!(wt.exists());
    assert!(ref_exists(&fx.repo, "refs/heads/aaf/t_am"));
    assert!(ref_exists(&fx.repo, "refs/aaf/snapshots/t_am/turn-1"));
    assert_eq!(
        Db::open(&fx.data.join("aaf.db"))
            .unwrap()
            .get_task("t_am")
            .unwrap()
            .state,
        TaskState::Review
    );

    // 2. pr.head_ref_oid != expect_head must make zero disk/DB changes.
    let wrong_head = "a".repeat(40);
    let (code, json, stderr) = fx.run_aaf(
        &fx.repo,
        &["merge", "11", "--expect-head", &wrong_head, "--json"],
    );
    assert_eq!(code, 0, "json={json} stderr={stderr}");
    assert_eq!(json["status"], "ALREADY_MERGED");
    assert!(wt.exists());
    assert!(ref_exists(&fx.repo, "refs/heads/aaf/t_am"));
    assert!(ref_exists(&fx.repo, "refs/aaf/snapshots/t_am/turn-1"));
    assert_eq!(
        Db::open(&fx.data.join("aaf.db"))
            .unwrap()
            .get_task("t_am")
            .unwrap()
            .state,
        TaskState::Review
    );

    // 3. Verified AlreadyMerged (not dry-run, matching head, mergeCommit ancestor of origin/main)
    //    cleans local artifacts and transitions Review -> Done.
    let (code, json, stderr) =
        fx.run_aaf(&fx.repo, &["merge", "11", "--expect-head", &head, "--json"]);
    assert_eq!(code, 0, "json={json} stderr={stderr}");
    assert_eq!(json["status"], "ALREADY_MERGED");
    assert!(!wt.exists(), "verified AlreadyMerged must remove worktree");
    assert!(!ref_exists(&fx.repo, "refs/heads/aaf/t_am"));
    assert!(!ref_exists(&fx.repo, "refs/aaf/snapshots/t_am/turn-1"));
    assert_eq!(
        Db::open(&fx.data.join("aaf.db"))
            .unwrap()
            .get_task("t_am")
            .unwrap()
            .state,
        TaskState::Done
    );
}

#[test]
fn c3_merge_stops_cleanup_for_active_task_and_does_not_create_db() {
    // Active task (Running) must prevent worktree/branch/snapshot deletion.
    let fx_active = Fixture::new();
    let (wt_active, head_active) =
        fx_active.create_task_pr("t_running", 12, Some(TaskState::Running));
    fx_active.write_gh_stub(12, "OPEN", &head_active, "aaf/t_running", false, None);

    let (code, json, stderr) = fx_active.run_aaf(
        &fx_active.repo,
        &["merge", "12", "--expect-head", &head_active, "--json"],
    );
    assert_eq!(code, 0, "json={json} stderr={stderr}");
    assert_eq!(json["status"], "MERGED");
    assert!(
        wt_active.exists(),
        "active task worktree must not be deleted"
    );
    assert!(ref_exists(&fx_active.repo, "refs/heads/aaf/t_running"));
    assert!(ref_exists(
        &fx_active.repo,
        "refs/aaf/snapshots/t_running/turn-1"
    ));
    assert_eq!(
        Db::open(&fx_active.data.join("aaf.db"))
            .unwrap()
            .get_task("t_running")
            .unwrap()
            .state,
        TaskState::Running
    );

    // When aaf.db does not exist, merge cleanup must NOT create aaf.db.
    let fx_nodb = Fixture::new();
    let (wt_nodb, head_nodb) = fx_nodb.create_task_pr("t_nodb", 13, None);
    fx_nodb.write_gh_stub(13, "OPEN", &head_nodb, "aaf/t_nodb", false, None);

    let (code, json, stderr) = fx_nodb.run_aaf(
        &fx_nodb.repo,
        &["merge", "13", "--expect-head", &head_nodb, "--json"],
    );
    assert_eq!(code, 0, "json={json} stderr={stderr}");
    assert_eq!(json["status"], "MERGED");
    assert!(
        !fx_nodb.data.join("aaf.db").exists(),
        "aaf merge cleanup must never create aaf.db"
    );
    assert!(!wt_nodb.exists());
    assert!(!ref_exists(&fx_nodb.repo, "refs/heads/aaf/t_nodb"));
    assert!(!ref_exists(
        &fx_nodb.repo,
        "refs/aaf/snapshots/t_nodb/turn-1"
    ));
}
