#![allow(clippy::unwrap_used, clippy::expect_used)]
//! A13 R10-L9-1: a task branch the user checked out in a LINKED worktree (the
//! task's repo root, not the main worktree) is not the task's aaf worktree.
//! `aaf gc` and the orphan batch sweep keep it with a warning instead of
//! running `git worktree remove` on the user's checkout (which deletes its
//! ignored files) and then aborting on a vanished working directory.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use aaf_engine::db::Db;
use aaf_engine::gc::{GcOptions, run_gc};
use aaf_engine::git::worktree;

#[path = "gc_support.rs"]
mod gc_support;
use gc_support::{done_task, git, has_branch, init_repo};

/// A main repo with `.env` ignored, and a linked worktree `feature` on `dev`
/// that holds `.env` and has `branch` checked out. Returns the feature path.
fn linked_checkout(root: &Path, branch: &str) -> PathBuf {
    let repo = root.join("repo");
    init_repo(&repo);
    fs::write(repo.join(".gitignore"), ".env\n").unwrap();
    git(&repo, &["add", ".gitignore"]);
    git(&repo, &["commit", "-q", "-m", "ignore env"]);
    let feature = root.join("feature");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            feature.to_str().unwrap(),
            "-b",
            "dev",
        ],
    );
    git(&feature, &["checkout", "-q", "-b", branch]);
    fs::write(feature.join(".env"), "SECRET=1\n").unwrap();
    feature
}

fn gc_fixture(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let feature = linked_checkout(root, "aaf/task-x");
    let data = root.join("data");
    fs::create_dir_all(&data).unwrap();
    let db = Db::open(&data.join("aaf.db")).unwrap();
    done_task(&db, "task-x", &feature);
    drop(db);
    (feature, data, root.join("worktrees"))
}

fn kept_warning(warnings: &[String], id: &str) -> bool {
    let prefix = format!("{id}: branch is checked out");
    warnings.iter().any(|w| w.starts_with(&prefix))
}

#[test]
fn gc_keeps_a_task_branch_checked_out_in_a_linked_user_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let (feature, data, worktrees) = gc_fixture(tmp.path());

    let opts = GcOptions {
        repo_root: Some(feature.clone()),
        ..GcOptions::default()
    };
    let report = run_gc(&data, &worktrees, &opts).unwrap();

    assert!(feature.join(".env").exists(), "the user's ignored file");
    assert!(has_branch(&feature, "aaf/task-x"), "branch kept");
    assert!(report.removed_worktrees.is_empty(), "{report:?}");
    assert!(report.removed_branches.is_empty(), "{report:?}");
    assert!(kept_warning(&report.warnings, "task-x"), "{report:?}");
}

#[test]
fn gc_dry_run_does_not_list_a_linked_user_worktree_as_removed() {
    let tmp = tempfile::tempdir().unwrap();
    let (feature, data, worktrees) = gc_fixture(tmp.path());

    let opts = GcOptions {
        repo_root: Some(feature.clone()),
        dry_run: true,
        ..GcOptions::default()
    };
    let dry = run_gc(&data, &worktrees, &opts).unwrap();

    assert!(dry.removed_worktrees.is_empty(), "{dry:?}");
    assert!(dry.removed_branches.is_empty(), "{dry:?}");
    assert!(kept_warning(&dry.warnings, "task-x"), "{dry:?}");
    assert!(feature.join(".env").exists());
}

#[test]
fn gc_still_removes_the_tasks_own_aaf_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let data = tmp.path().join("data");
    let worktrees = tmp.path().join("worktrees");
    init_repo(&repo);
    let wt = worktree::create(&repo, &worktrees, "task-y", "main").unwrap();
    fs::create_dir_all(&data).unwrap();
    let db = Db::open(&data.join("aaf.db")).unwrap();
    done_task(&db, "task-y", &repo);
    drop(db);

    let opts = GcOptions {
        repo_root: Some(repo.clone()),
        ..GcOptions::default()
    };
    let report = run_gc(&data, &worktrees, &opts).unwrap();

    assert!(!wt.path.exists(), "{report:?}");
    assert!(!has_branch(&repo, "aaf/task-y"), "{report:?}");
    assert!(!kept_warning(&report.warnings, "task-y"), "{report:?}");
}

#[test]
fn orphan_sweep_keeps_a_batch_branch_checked_out_in_a_linked_user_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let feature = linked_checkout(tmp.path(), "aaf/batch-x");
    let worktrees = tmp.path().join("worktrees");
    let repo_id = aaf_fp::repo_id(&feature);

    let dry = worktree::remove_orphan_batch_worktrees(
        &feature,
        &worktrees,
        &repo_id,
        &HashSet::new(),
        true,
    )
    .unwrap();
    assert!(dry.removed.is_empty(), "{dry:?}");

    let sweep = worktree::remove_orphan_batch_worktrees(
        &feature,
        &worktrees,
        &repo_id,
        &HashSet::new(),
        false,
    )
    .unwrap();

    assert!(feature.join(".env").exists(), "the user's ignored file");
    assert!(has_branch(&feature, "aaf/batch-x"), "branch kept");
    assert!(sweep.removed.is_empty(), "{sweep:?}");
    assert!(kept_warning(&sweep.warnings, "batch-x"), "{sweep:?}");
}
