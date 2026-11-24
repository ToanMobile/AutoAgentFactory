#![allow(clippy::unwrap_used, clippy::expect_used)]
//! A13 R8-L9-1: a locked task worktree whose directory is gone (git's own use
//! case for a lock: a tree on a volume that is not mounted) keeps its record
//! through `git worktree prune`, so git refuses to delete its branch. That is
//! a per-task warning keeping the branch, not an error aborting the whole
//! `aaf gc` run; the dry run says the same instead of listing the branch.

use std::collections::HashSet;
use std::fs;

use aaf_engine::db::Db;
use aaf_engine::factory::{GitOps, RealGit, resolve_base};
use aaf_engine::gc::{GcOptions, run_gc};
use aaf_engine::git::worktree;

#[path = "gc_support.rs"]
mod gc_support;
use gc_support::{done_task, expired_stream, git, has_branch, init_repo};

#[test]
fn gc_keeps_locked_worktree_with_missing_directory_and_finishes_the_run() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let data = tmp.path().join("data");
    let worktrees = tmp.path().join("worktrees");
    init_repo(&repo);
    let a = worktree::create(&repo, &worktrees, "task-a", "main").unwrap();
    git(&repo, &["worktree", "lock", a.path.to_str().unwrap()]);
    let b = worktree::create(&repo, &worktrees, "task-b", "main").unwrap();
    // The volume holding task-a's tree is not mounted.
    fs::rename(&a.path, tmp.path().join("unmounted")).unwrap();

    fs::create_dir_all(&data).unwrap();
    let db = Db::open(&data.join("aaf.db")).unwrap();
    done_task(&db, "task-a", &repo);
    done_task(&db, "task-b", &repo);
    drop(db);
    let old = expired_stream(&data);

    let mut opts = GcOptions {
        repo_root: Some(repo.clone()),
        dry_run: true,
        ..GcOptions::default()
    };
    let dry = run_gc(&data, &worktrees, &opts).unwrap();
    assert!(
        dry.warnings.iter().any(|w| w.starts_with("task-a:")),
        "{:?}",
        dry.warnings
    );
    assert_eq!(dry.removed_branches, vec!["aaf/task-b".to_string()]);
    assert!(b.path.is_dir() && old.exists());

    opts.dry_run = false;
    let report = run_gc(&data, &worktrees, &opts).unwrap();
    assert!(
        report.warnings.iter().any(|w| w.starts_with("task-a:")),
        "{:?}",
        report.warnings
    );
    assert!(
        has_branch(&repo, "aaf/task-a"),
        "task-a branch must be kept"
    );
    assert!(!b.path.exists(), "task-b worktree must be removed");
    assert!(!has_branch(&repo, "aaf/task-b"), "task-b branch must go");
    assert!(!old.exists(), "expired stream file must be swept");
}

/// The startup orphan-batch sweep keeps a locked batch worktree whose
/// directory is gone, with a warning, and still sweeps the other batches.
#[test]
fn orphan_sweep_keeps_locked_batch_with_missing_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let worktrees = tmp.path().join("worktrees");
    init_repo(&repo);
    let batch_a = worktree::create(&repo, &worktrees, "batch-a", "main").unwrap();
    git(&repo, &["worktree", "lock", batch_a.path.to_str().unwrap()]);
    fs::rename(&batch_a.path, tmp.path().join("unmounted")).unwrap();
    let batch_b = worktree::create(&repo, &worktrees, "batch-b", "main").unwrap();

    let base = resolve_base(&repo, "main").unwrap();
    let real = RealGit::new(repo.clone(), worktrees, tmp.path().join("locks"), base);
    let warnings = real.sweep_orphan_batches(&HashSet::new()).unwrap();

    assert!(
        warnings.iter().any(|w| w.starts_with("batch-a:")),
        "{warnings:?}"
    );
    assert!(has_branch(&repo, "aaf/batch-a"), "locked batch branch kept");
    assert!(!batch_b.path.exists(), "clean orphan batch must be swept");
    assert!(!has_branch(&repo, "aaf/batch-b"), "batch-b branch must go");
}

/// A13 R8 (Lane 4 note): an unregistered orphan batch directory git cannot
/// read (a dangling `.git` link) is kept like a dirty one, not deleted as
/// if clean.
#[test]
fn orphan_sweep_keeps_unreadable_unregistered_batch_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let worktrees = tmp.path().join("worktrees");
    init_repo(&repo);
    let batch_b = worktree::create(&repo, &worktrees, "batch-b", "main").unwrap();
    git(&repo, &["branch", "aaf/batch-c", "main"]);
    let dangling = batch_b.path.parent().unwrap().join("batch-c");
    fs::create_dir_all(&dangling).unwrap();
    let link = tmp.path().join("gone");
    fs::write(
        dangling.join(".git"),
        format!("gitdir: {}\n", link.display()),
    )
    .unwrap();
    fs::write(dangling.join("work.txt"), "x\n").unwrap();

    let base = resolve_base(&repo, "main").unwrap();
    let real = RealGit::new(repo.clone(), worktrees, tmp.path().join("locks"), base);
    real.sweep_orphan_batches(&HashSet::new()).unwrap();

    assert!(
        dangling.join("work.txt").exists(),
        "unreadable batch directory must be kept"
    );
    assert!(!batch_b.path.exists(), "clean orphan batch must be swept");
}
