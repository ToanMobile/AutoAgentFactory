#![allow(clippy::unwrap_used, clippy::expect_used)]
//! SEC-136 verification: host-side ref deletion (`run_gc`, `cleanup_missing_worktree`)
//! must fail closed when `<common>/logs/refs/heads/aaf` is a symlink to an outside
//! directory, without unlinking files outside the repository.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use aaf_engine::db::Db;
use aaf_engine::gc::{GcOptions, run_gc};
use aaf_engine::git::worktree::cleanup_missing_worktree;

#[path = "gc_support.rs"]
mod gc_support;
use gc_support::{done_task, git, has_branch, init_repo};

fn common_git_dir(repo: &Path) -> PathBuf {
    let raw = git(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    );
    PathBuf::from(raw.trim())
}

fn setup_task_branch(repo: &Path, task_id: &str) -> String {
    init_repo(repo);
    git(repo, &["config", "core.logAllRefUpdates", "true"]);
    let branch = format!("aaf/{task_id}");
    git(repo, &["branch", &branch, "main"]);
    assert!(has_branch(repo, &branch), "task branch should exist");
    branch
}

fn plant_reflog_dir_symlink(repo: &Path, outside: &Path, task_id: &str) -> PathBuf {
    fs::create_dir_all(outside).unwrap();
    let victim = outside.join(task_id);
    fs::write(&victim, "victim\n").unwrap();

    let aaf_logs_dir = common_git_dir(repo).join("logs/refs/heads/aaf");
    assert!(
        aaf_logs_dir.is_dir(),
        "expected git branch to create reflog dir {}",
        aaf_logs_dir.display()
    );
    fs::remove_dir_all(&aaf_logs_dir).unwrap();
    symlink(outside, &aaf_logs_dir).unwrap();
    victim
}

#[test]
fn gc_task_branch_delete_does_not_unlink_outside_file() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let data = tmp.path().join("data");
    let worktrees = tmp.path().join("worktrees");
    let task_id = "sec136-gc";

    setup_task_branch(&repo, task_id);

    fs::create_dir_all(&data).unwrap();
    let db = Db::open(&data.join("aaf.db")).unwrap();
    done_task(&db, task_id, &repo);
    drop(db);

    let victim = plant_reflog_dir_symlink(&repo, &tmp.path().join("outside"), task_id);

    let opts = GcOptions {
        repo_root: Some(repo.clone()),
        ..GcOptions::default()
    };
    let _ = run_gc(&data, &worktrees, &opts);

    assert!(
        victim.exists(),
        "SEC-136: run_gc must not unlink outside file {} via reflog directory symlink",
        victim.display()
    );
    assert_eq!(
        fs::read_to_string(&victim).unwrap(),
        "victim\n",
        "SEC-136: outside file content must remain intact"
    );
}

#[test]
fn cleanup_missing_worktree_does_not_unlink_outside_file() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let worktrees = tmp.path().join("worktrees");
    let task_id = "sec136-wt";

    setup_task_branch(&repo, task_id);
    let victim = plant_reflog_dir_symlink(&repo, &tmp.path().join("outside"), task_id);

    let res = cleanup_missing_worktree(&repo, &worktrees, task_id, "main");

    assert!(
        victim.exists(),
        "SEC-136: cleanup_missing_worktree must not unlink outside file {} via reflog directory symlink",
        victim.display()
    );
    assert_eq!(
        fs::read_to_string(&victim).unwrap(),
        "victim\n",
        "SEC-136: outside file content must remain intact"
    );
    assert!(
        res.is_err(),
        "SEC-136: cleanup_missing_worktree must fail closed when logs/ contains a symlink"
    );
}

#[test]
fn gc_task_branch_delete_with_plain_reflogs_still_deletes() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let data = tmp.path().join("data");
    let worktrees = tmp.path().join("worktrees");
    let task_id = "sec136-plain-gc";

    let branch = setup_task_branch(&repo, task_id);

    fs::create_dir_all(&data).unwrap();
    let db = Db::open(&data.join("aaf.db")).unwrap();
    done_task(&db, task_id, &repo);
    drop(db);

    let opts = GcOptions {
        repo_root: Some(repo.clone()),
        ..GcOptions::default()
    };
    let report = run_gc(&data, &worktrees, &opts).unwrap();

    assert!(
        !has_branch(&repo, &branch),
        "plain task branch {branch} must be deleted by run_gc"
    );
    assert!(
        report.removed_branches.contains(&branch),
        "run_gc report must record deleted branch {branch}; report={report:?}"
    );
    assert!(has_branch(&repo, "main"), "main branch must remain intact");
}

#[test]
fn cleanup_missing_worktree_with_plain_reflogs_still_deletes() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let worktrees = tmp.path().join("worktrees");
    let task_id = "sec136-plain-wt";

    let branch = setup_task_branch(&repo, task_id);

    cleanup_missing_worktree(&repo, &worktrees, task_id, "main").unwrap();

    assert!(
        !has_branch(&repo, &branch),
        "plain task branch {branch} must be deleted by cleanup_missing_worktree"
    );
    assert!(has_branch(&repo, "main"), "main branch must remain intact");
}
