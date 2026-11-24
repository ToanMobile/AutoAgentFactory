#![allow(clippy::unwrap_used, clippy::expect_used)]
//! A13 R9-L9-1: a ref git cannot update (a stale `<ref>.lock` left by a
//! crashed git) is a per-task warning in `aaf gc` and a per-id warning in the
//! orphan batch sweep, not an error aborting the whole run.
//! A13 R9-L9-2: a task branch checked out in the main worktree is kept with a
//! warning; the dry run no longer lists the main checkout as removed.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use aaf_engine::db::Db;
use aaf_engine::factory::{GitOps, RealGit, resolve_base};
use aaf_engine::gc::{GcOptions, run_gc};
use aaf_engine::git::worktree;

#[path = "gc_support.rs"]
mod gc_support;
use gc_support::{done_task, expired_stream, git, init_repo};

fn has_ref(repo: &Path, name: &str) -> bool {
    !git(repo, &["for-each-ref", "--format=%(refname)", name])
        .trim()
        .is_empty()
}

/// A stale lock file beside the loose ref `name`, as a crashed git leaves.
fn lock_ref(repo: &Path, name: &str) {
    let git_dir = PathBuf::from(git(repo, &["rev-parse", "--absolute-git-dir"]).trim());
    let lock = git_dir.join(format!("{name}.lock"));
    fs::create_dir_all(lock.parent().unwrap()).unwrap();
    fs::write(lock, "").unwrap();
}

/// Two Done tasks with worktrees, `task-a` with a snapshot ref, and an
/// expired stream file. Returns (data dir, worktrees dir, task-b path, stream).
fn two_done_tasks(root: &Path, repo: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let data = root.join("data");
    let worktrees = root.join("worktrees");
    init_repo(repo);
    worktree::create(repo, &worktrees, "task-a", "main").unwrap();
    let b = worktree::create(repo, &worktrees, "task-b", "main").unwrap();
    git(repo, &["update-ref", "refs/aaf/snapshots/task-a/1", "HEAD"]);
    fs::create_dir_all(&data).unwrap();
    let db = Db::open(&data.join("aaf.db")).unwrap();
    done_task(&db, "task-a", repo);
    done_task(&db, "task-b", repo);
    drop(db);
    let old = expired_stream(&data);
    (data, worktrees, b.path, old)
}

#[test]
fn gc_warns_on_a_locked_branch_ref_and_finishes_the_run() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let (data, worktrees, b, old) = two_done_tasks(tmp.path(), &repo);
    lock_ref(&repo, "refs/heads/aaf/task-a");

    let opts = GcOptions {
        repo_root: Some(repo.clone()),
        ..GcOptions::default()
    };
    let report = run_gc(&data, &worktrees, &opts).unwrap();

    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.starts_with("task-a: branch delete:")),
        "{:?}",
        report.warnings
    );
    assert!(
        has_ref(&repo, "refs/heads/aaf/task-a"),
        "task-a branch kept"
    );
    assert!(
        has_ref(&repo, "refs/aaf/snapshots/task-a/1"),
        "task-a snapshot refs kept with its branch"
    );
    assert!(!b.exists(), "task-b worktree must be removed");
    assert!(
        !has_ref(&repo, "refs/heads/aaf/task-b"),
        "task-b branch must go"
    );
    assert!(!old.exists(), "expired stream file must be swept");
}

#[test]
fn gc_warns_on_a_locked_snapshot_ref_and_finishes_the_run() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let (data, worktrees, b, old) = two_done_tasks(tmp.path(), &repo);
    lock_ref(&repo, "refs/aaf/snapshots/task-a/1");

    let opts = GcOptions {
        repo_root: Some(repo.clone()),
        ..GcOptions::default()
    };
    let report = run_gc(&data, &worktrees, &opts).unwrap();

    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.starts_with("task-a: snapshot refs:")),
        "{:?}",
        report.warnings
    );
    assert!(!b.exists(), "task-b worktree must be removed");
    assert!(
        !has_ref(&repo, "refs/heads/aaf/task-b"),
        "task-b branch must go"
    );
    assert!(!old.exists(), "expired stream file must be swept");
}

#[test]
fn orphan_sweep_warns_on_a_locked_batch_branch_and_sweeps_the_rest() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let worktrees = tmp.path().join("worktrees");
    init_repo(&repo);
    worktree::create(&repo, &worktrees, "batch-a", "main").unwrap();
    let batch_b = worktree::create(&repo, &worktrees, "batch-b", "main").unwrap();
    lock_ref(&repo, "refs/heads/aaf/batch-a");

    let base = resolve_base(&repo, "main").unwrap();
    let real = RealGit::new(repo.clone(), worktrees, tmp.path().join("locks"), base);
    let warnings = real.sweep_orphan_batches(&HashSet::new()).unwrap();

    assert!(
        warnings
            .iter()
            .any(|w| w.starts_with("batch-a: branch delete:")),
        "{warnings:?}"
    );
    assert!(!batch_b.path.exists(), "batch-b worktree must be swept");
    assert!(
        !has_ref(&repo, "refs/heads/aaf/batch-b"),
        "batch-b branch must go"
    );
}

#[test]
fn gc_keeps_a_task_branch_checked_out_in_the_main_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let data = tmp.path().join("data");
    let worktrees = tmp.path().join("worktrees");
    init_repo(&repo);
    git(&repo, &["checkout", "-q", "-b", "aaf/task-x"]);
    fs::create_dir_all(&data).unwrap();
    let db = Db::open(&data.join("aaf.db")).unwrap();
    done_task(&db, "task-x", &repo);
    drop(db);

    let mut opts = GcOptions {
        repo_root: Some(repo.clone()),
        dry_run: true,
        ..GcOptions::default()
    };
    let dry = run_gc(&data, &worktrees, &opts).unwrap();
    assert!(dry.removed_worktrees.is_empty(), "{dry:?}");
    assert!(dry.removed_branches.is_empty(), "{dry:?}");
    assert!(
        dry.warnings
            .iter()
            .any(|w| w.starts_with("task-x: branch is checked out in the main worktree")),
        "{:?}",
        dry.warnings
    );

    opts.dry_run = false;
    let report = run_gc(&data, &worktrees, &opts).unwrap();
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.starts_with("task-x: branch is checked out in the main worktree")),
        "{:?}",
        report.warnings
    );
    assert!(repo.join("README.md").exists());
    assert!(has_ref(&repo, "refs/heads/aaf/task-x"), "branch kept");
}
