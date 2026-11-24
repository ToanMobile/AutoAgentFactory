#![allow(clippy::unwrap_used, clippy::expect_used)]
//! A13 L9-R5-1: a worktree `git worktree remove` refuses without `--force`
//! (here: initialised submodules) is a per-task warning, not an error that
//! aborts the whole `aaf gc` run (later tasks, orphan batches and the expired
//! artifact sweep).

use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::process::Command;

use aaf_engine::db::Db;
use aaf_engine::factory::{GitOps, RealGit, resolve_base};
use aaf_engine::gc::{GcOptions, run_gc};
use aaf_engine::git::worktree;

#[path = "gc_support.rs"]
mod gc_support;
use gc_support::{done_task, expired_stream, git, has_branch, init_repo};

#[test]
fn gc_warns_on_unremovable_worktree_and_finishes_the_run() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let upstream = tmp.path().join("upstream");
    let data = tmp.path().join("data");
    let worktrees = tmp.path().join("worktrees");
    init_repo(&upstream);
    init_repo(&repo);
    let upstream_str = upstream.to_str().unwrap();
    git(&repo, &["submodule", "add", "-q", upstream_str, "sub"]);
    git(&repo, &["commit", "-q", "-m", "add submodule"]);

    // Task A's worktree has its submodule initialised; B's does not.
    let a = worktree::create(&repo, &worktrees, "task-a", "main").unwrap();
    git(&a.path, &["submodule", "update", "--init", "-q"]);
    assert!(git(&a.path, &["status", "--porcelain"]).trim().is_empty());
    let b = worktree::create(&repo, &worktrees, "task-b", "main").unwrap();
    // Orphan batches: the first refuses for the same reason, the second
    // is clean and must still be swept.
    let batch_a = worktree::create(&repo, &worktrees, "batch-a", "main").unwrap();
    git(&batch_a.path, &["submodule", "update", "--init", "-q"]);
    let batch_b = worktree::create(&repo, &worktrees, "batch-b", "main").unwrap();

    fs::create_dir_all(&data).unwrap();
    let db = Db::open(&data.join("aaf.db")).unwrap();
    done_task(&db, "task-a", &repo);
    done_task(&db, "task-b", &repo);
    drop(db);

    let old = expired_stream(&data);

    let opts = GcOptions {
        repo_root: Some(repo.clone()),
        ..GcOptions::default()
    };
    let report = run_gc(&data, &worktrees, &opts).unwrap();

    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.starts_with("task-a: worktree remove:")),
        "{:?}",
        report.warnings
    );
    assert!(a.path.is_dir(), "task-a worktree must be kept");
    assert!(
        has_branch(&repo, "aaf/task-a"),
        "task-a branch must be kept"
    );
    assert!(!b.path.exists(), "task-b worktree must be removed");
    assert!(!has_branch(&repo, "aaf/task-b"), "task-b branch must go");
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.starts_with("batch-a: worktree remove:")),
        "refused orphan batch must be reported: {:?}",
        report.warnings
    );
    assert!(batch_a.path.is_dir(), "refused orphan batch must be kept");
    assert!(has_branch(&repo, "aaf/batch-a"));
    assert_eq!(report.removed_orphan_batches, vec!["batch-b".to_string()]);
    assert!(!batch_b.path.exists());
    assert!(!old.exists(), "expired stream file must be swept");
}

/// A13 L4-R6-1: the scheduler's startup sweep (the `GitOps` port) reports a
/// refused orphan batch instead of keeping it silently forever.
#[test]
fn startup_orphan_sweep_reports_refused_batch_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let upstream = tmp.path().join("upstream");
    let worktrees = tmp.path().join("worktrees");
    init_repo(&upstream);
    init_repo(&repo);
    git(
        &repo,
        &["submodule", "add", "-q", upstream.to_str().unwrap(), "sub"],
    );
    git(&repo, &["commit", "-q", "-m", "add submodule"]);
    let batch_a = worktree::create(&repo, &worktrees, "batch-a", "main").unwrap();
    git(&batch_a.path, &["submodule", "update", "--init", "-q"]);
    let batch_b = worktree::create(&repo, &worktrees, "batch-b", "main").unwrap();

    let base = resolve_base(&repo, "main").unwrap();
    let real = RealGit::new(repo.clone(), worktrees, tmp.path().join("locks"), base);
    let warnings = real.sweep_orphan_batches(&HashSet::new()).unwrap();

    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].starts_with("batch-a: worktree remove:"),
        "{warnings:?}"
    );
    assert!(batch_a.path.is_dir(), "refused orphan batch must be kept");
    assert!(!batch_b.path.exists(), "clean orphan batch must be swept");
}

/// A13 L12-R6 (cosmetic): `gc --dry-run` reports the worktrees the real run
/// would be refused (initialised submodules, a lock) as warnings, not as
/// removed.
#[test]
fn gc_dry_run_warns_on_worktrees_git_would_refuse() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let upstream = tmp.path().join("upstream");
    let data = tmp.path().join("data");
    let worktrees = tmp.path().join("worktrees");
    init_repo(&upstream);
    init_repo(&repo);
    git(
        &repo,
        &["submodule", "add", "-q", upstream.to_str().unwrap(), "sub"],
    );
    git(&repo, &["commit", "-q", "-m", "add submodule"]);

    let a = worktree::create(&repo, &worktrees, "task-a", "main").unwrap();
    git(&a.path, &["submodule", "update", "--init", "-q"]);
    let b = worktree::create(&repo, &worktrees, "task-b", "main").unwrap();
    let c = worktree::create(&repo, &worktrees, "task-c", "main").unwrap();
    git(&repo, &["worktree", "lock", c.path.to_str().unwrap()]);
    let batch_a = worktree::create(&repo, &worktrees, "batch-a", "main").unwrap();
    git(&batch_a.path, &["submodule", "update", "--init", "-q"]);
    let batch_b = worktree::create(&repo, &worktrees, "batch-b", "main").unwrap();
    let batch_c = worktree::create(&repo, &worktrees, "batch-c", "main").unwrap();
    git(&repo, &["worktree", "lock", batch_c.path.to_str().unwrap()]);

    fs::create_dir_all(&data).unwrap();
    let db = Db::open(&data.join("aaf.db")).unwrap();
    for id in ["task-a", "task-b", "task-c"] {
        done_task(&db, id, &repo);
    }
    drop(db);

    let opts = GcOptions {
        repo_root: Some(repo.clone()),
        dry_run: true,
        ..GcOptions::default()
    };
    let report = run_gc(&data, &worktrees, &opts).unwrap();

    for id in ["task-a", "task-c", "batch-a", "batch-c"] {
        let prefix = format!("{id}: worktree remove:");
        assert!(
            report.warnings.iter().any(|w| w.starts_with(&prefix)),
            "{id}: {:?}",
            report.warnings
        );
    }
    assert_eq!(report.removed_worktrees.len(), 1, "{report:?}");
    assert!(
        report.removed_worktrees[0].ends_with("/task-b"),
        "{report:?}"
    );
    assert_eq!(report.removed_branches, vec!["aaf/task-b".to_string()]);
    assert_eq!(report.removed_orphan_batches, vec!["batch-b".to_string()]);
    // A dry run changes nothing.
    for path in [&a.path, &b.path, &c.path, &batch_a.path, &batch_b.path] {
        assert!(path.is_dir(), "{}", path.display());
    }
}

/// A13 R6b: `gc --force` passes one `--force`, which git still refuses for a
/// locked worktree (a lock is explicit user intent; no `-f -f`). That is a
/// per-task warning, not an error aborting the run, and a forced dry run
/// reports it as would-be-refused.
#[test]
fn gc_force_warns_on_locked_worktree_and_finishes_the_run() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let data = tmp.path().join("data");
    let worktrees = tmp.path().join("worktrees");
    init_repo(&repo);
    let a = worktree::create(&repo, &worktrees, "task-a", "main").unwrap();
    git(&repo, &["worktree", "lock", a.path.to_str().unwrap()]);
    let b = worktree::create(&repo, &worktrees, "task-b", "main").unwrap();

    fs::create_dir_all(&data).unwrap();
    let db = Db::open(&data.join("aaf.db")).unwrap();
    done_task(&db, "task-a", &repo);
    done_task(&db, "task-b", &repo);
    drop(db);
    let old = expired_stream(&data);

    let mut opts = GcOptions {
        repo_root: Some(repo.clone()),
        force: true,
        dry_run: true,
        ..GcOptions::default()
    };
    let dry = run_gc(&data, &worktrees, &opts).unwrap();
    assert!(
        dry.warnings.iter().any(
            |w| w.starts_with("task-a: worktree remove: would be refused:") && w.contains("locked")
        ),
        "{:?}",
        dry.warnings
    );
    assert_eq!(dry.removed_worktrees.len(), 1, "{dry:?}");
    assert!(dry.removed_worktrees[0].ends_with("/task-b"), "{dry:?}");
    assert_eq!(dry.removed_branches, vec!["aaf/task-b".to_string()]);
    assert!(a.path.is_dir() && b.path.is_dir() && old.exists());

    opts.dry_run = false;
    let report = run_gc(&data, &worktrees, &opts).unwrap();
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.starts_with("task-a: worktree remove:")),
        "{:?}",
        report.warnings
    );
    assert!(a.path.is_dir(), "locked task-a worktree must be kept");
    assert!(
        has_branch(&repo, "aaf/task-a"),
        "task-a branch must be kept"
    );
    assert!(!b.path.exists(), "task-b worktree must be removed");
    assert!(!has_branch(&repo, "aaf/task-b"), "task-b branch must go");
    assert!(!old.exists(), "expired stream file must be swept");
}

/// A13 L9-R7-1: a registered task worktree whose `.git` link is broken (the
/// directory was replaced) makes the forced dry run's refusal probe fail.
/// That is a per-task "would be refused" warning, like the real run's
/// warning, not an error aborting the whole dry run.
#[test]
fn gc_force_dry_run_warns_on_broken_worktree_link_and_finishes() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let data = tmp.path().join("data");
    let worktrees = tmp.path().join("worktrees");
    init_repo(&repo);
    let b = worktree::create(&repo, &worktrees, "task-b", "main").unwrap();
    let d = worktree::create(&repo, &worktrees, "task-d", "main").unwrap();
    fs::rename(&d.path, tmp.path().join("task-d-moved")).unwrap();
    fs::create_dir(&d.path).unwrap();

    fs::create_dir_all(&data).unwrap();
    let db = Db::open(&data.join("aaf.db")).unwrap();
    done_task(&db, "task-b", &repo);
    done_task(&db, "task-d", &repo);
    drop(db);

    let mut opts = GcOptions {
        repo_root: Some(repo.clone()),
        force: true,
        dry_run: true,
        ..GcOptions::default()
    };
    let dry = run_gc(&data, &worktrees, &opts).unwrap();
    assert!(
        dry.warnings
            .iter()
            .any(|w| w.starts_with("task-d: worktree remove: would be refused:")),
        "{:?}",
        dry.warnings
    );
    assert_eq!(dry.removed_worktrees.len(), 1, "{dry:?}");
    assert!(dry.removed_worktrees[0].ends_with("/task-b"), "{dry:?}");
    assert_eq!(dry.removed_branches, vec!["aaf/task-b".to_string()]);
    assert!(b.path.is_dir() && d.path.is_dir());

    opts.dry_run = false;
    let report = run_gc(&data, &worktrees, &opts).unwrap();
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.starts_with("task-d: worktree remove:")),
        "{:?}",
        report.warnings
    );
    assert!(d.path.is_dir(), "task-d directory must be kept");
    assert!(
        has_branch(&repo, "aaf/task-d"),
        "task-d branch must be kept"
    );
    assert!(!b.path.exists(), "task-b worktree must be removed");
    assert!(!has_branch(&repo, "aaf/task-b"), "task-b branch must go");
}

/// `--force` ignores initialised submodules, so a forced dry run reports a
/// submodule-initialised task worktree as removable (the probe honours
/// `force`).
#[test]
fn gc_force_dry_run_reports_submodule_worktree_as_removable() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let upstream = tmp.path().join("upstream");
    let data = tmp.path().join("data");
    let worktrees = tmp.path().join("worktrees");
    init_repo(&upstream);
    init_repo(&repo);
    git(
        &repo,
        &["submodule", "add", "-q", upstream.to_str().unwrap(), "sub"],
    );
    git(&repo, &["commit", "-q", "-m", "add submodule"]);
    let a = worktree::create(&repo, &worktrees, "task-a", "main").unwrap();
    git(&a.path, &["submodule", "update", "--init", "-q"]);

    fs::create_dir_all(&data).unwrap();
    let db = Db::open(&data.join("aaf.db")).unwrap();
    done_task(&db, "task-a", &repo);
    drop(db);

    let opts = GcOptions {
        repo_root: Some(repo.clone()),
        force: true,
        dry_run: true,
        ..GcOptions::default()
    };
    let dry = run_gc(&data, &worktrees, &opts).unwrap();
    assert!(dry.warnings.is_empty(), "{:?}", dry.warnings);
    assert_eq!(dry.removed_worktrees.len(), 1, "{dry:?}");
    assert!(dry.removed_worktrees[0].ends_with("/task-a"), "{dry:?}");
    assert_eq!(dry.removed_branches, vec!["aaf/task-a".to_string()]);
    assert!(a.path.is_dir(), "a dry run changes nothing");
}

/// `worktree::removal_refusal` reports a checked-out gitlink (`<path>/.git`
/// present) even when the worktree's git dir has no `modules` directory,
/// mirroring git's own refusal.
#[test]
fn removal_refusal_reports_populated_gitlink_without_modules_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let upstream = tmp.path().join("upstream");
    let worktrees = tmp.path().join("worktrees");
    init_repo(&upstream);
    init_repo(&repo);
    git(
        &repo,
        &["submodule", "add", "-q", upstream.to_str().unwrap(), "sub"],
    );
    git(&repo, &["commit", "-q", "-m", "add submodule"]);
    let a = worktree::create(&repo, &worktrees, "task-a", "main").unwrap();
    assert_eq!(worktree::removal_refusal(&a.path).unwrap(), None);
    // An embedded repository checked out at the gitlink path, not through
    // `git submodule`, so no `<git-dir>/modules` exists.
    init_repo(&a.path.join("sub"));
    let git_dir = git(&a.path, &["rev-parse", "--absolute-git-dir"]);
    assert!(!Path::new(git_dir.trim()).join("modules").exists());

    let reason = worktree::removal_refusal(&a.path).unwrap();
    assert!(
        reason.is_some_and(|r| r.contains("submodules")),
        "{reason:?}"
    );
    // Git itself refuses the plain removal for the same reason.
    let out = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["worktree", "remove", a.path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("submodules"));
}
