#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for C4+C6 F1/F2 audit fixes:
//! - F1-3: `startup_sweep` (`clean_repo_offline_unlocked*`) and non-`--force`
//!   `aaf gc` refuse to delete a `Done` task's worktree or branch when its tip
//!   has unmerged commits outside the base ref or beyond `MergeResult.head_sha`.
//! - F1-4: `run_gc` locks the sorted set of `repo_id`s across the main repo,
//!   registered linked worktrees, and DB task roots, so a `.run.lock` held by
//!   `aaf run` from a linked worktree causes `run_gc` to skip the repo as busy.
//! - F2: `run_gc` also sweeps orphaned batch worktrees created under a linked
//!   worktree's `repo_id`.

#[path = "git_support.rs"]
mod support;

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{EventSink, TaskStore};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::{Db, private};
use aaf_engine::gc::{
    GcOptions, clean_repo_offline_unlocked, clean_repo_offline_unlocked_with_heads, run_gc,
};
use aaf_engine::git::worktree;
use serde_json::json;
use support::{Fixture, commit_file, git};

fn insert_done(db: &Db, repo: &Path, id: &str) {
    let mut task = Task::new_root(
        id,
        repo.to_string_lossy(),
        format!("Task {id}"),
        ImmutableSpec {
            description: "F1 regression fixture".to_string(),
            acceptance: vec!["ok".to_string()],
        },
        Priority::P2,
        1_700_000_000,
    );
    task.state = TaskState::Done;
    db.insert_task(&task).unwrap();
}

fn has_branch(repo: &Path, short: &str) -> bool {
    !git(repo, &["branch", "--list", short]).is_empty()
}

fn has_ref(repo: &Path, full_ref: &str) -> bool {
    !git(repo, &["for-each-ref", "--format=%(refname)", full_ref]).is_empty()
}

#[test]
fn f1_3_offline_sweep_guards_done_squash_extra_commit_and_cleans_verified_match() {
    let fx = Fixture::new();
    let data_dir = fx.repo.parent().unwrap().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    // Task A: Done squash-merged at H1, then user added unmerged commit H2 -> MUST KEEP.
    let wt_moved = worktree::create(&fx.repo, &fx.worktrees, "t-moved", "main").unwrap();
    let h1 = commit_file(&wt_moved.path, "a1.txt", "first\n", "commit H1");
    let h2 = commit_file(&wt_moved.path, "a2.txt", "second\n", "extra commit H2");
    git(
        &fx.repo,
        &["update-ref", "refs/aaf/snapshots/t-moved/s1", &h2],
    );

    // Task B: Done squash-merged at B1, still at B1 (matches MergeResult.head_sha) -> MUST CLEAN.
    let wt_match = worktree::create(&fx.repo, &fx.worktrees, "t-match", "main").unwrap();
    let b1 = commit_file(&wt_match.path, "b1.txt", "squashed\n", "commit B1");
    git(
        &fx.repo,
        &["update-ref", "refs/aaf/snapshots/t-match/s1", &b1],
    );
    // Simulate squash-merge commit on main (distinct commit SHA from B1).
    commit_file(&fx.repo, "b1.txt", "squashed\n", "squash PR B1");

    let done_tasks = vec![
        ("t-moved".to_string(), Some(h1.clone())),
        ("t-match".to_string(), Some(b1.clone())),
    ];
    let active: HashSet<String> = ["t-moved".to_string(), "t-match".to_string()]
        .into_iter()
        .collect();

    let report = clean_repo_offline_unlocked_with_heads(
        &fx.repo,
        &data_dir,
        &fx.worktrees,
        &done_tasks,
        &active,
        None,
    );

    assert!(
        wt_moved.path.is_dir(),
        "worktree with extra unmerged commit H2 must be kept"
    );
    assert!(
        has_branch(&fx.repo, "aaf/t-moved"),
        "branch with extra unmerged commit H2 must be kept"
    );
    assert_eq!(
        git(&fx.repo, &["rev-parse", "refs/heads/aaf/t-moved"]),
        h2,
        "commit H2 must remain intact on aaf/t-moved"
    );
    assert!(
        has_ref(&fx.repo, "refs/aaf/snapshots/t-moved/s1"),
        "snapshots must be kept when branch is kept"
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("t-moved") && w.contains(&h1)),
        "expected mismatch warning for t-moved, got {:?}",
        report.warnings
    );

    assert!(
        !wt_match.path.exists(),
        "verified squash-merged worktree matching head_sha must be removed"
    );
    assert!(
        !has_branch(&fx.repo, "aaf/t-match"),
        "verified squash-merged branch matching head_sha must be removed"
    );
    assert!(
        !has_ref(&fx.repo, "refs/aaf/snapshots/t-match/s1"),
        "verified squash-merged snapshots must be removed"
    );
}

#[test]
fn f1_3_offline_sweep_and_gc_without_expected_head_require_ancestry_in_base() {
    let fx = Fixture::new();
    let data_dir = fx.repo.parent().unwrap().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    // Task unmerged: Done with NO MergeResult.head_sha and unmerged commit -> MUST KEEP.
    let wt_unmerged = worktree::create(&fx.repo, &fx.worktrees, "t-unmerged", "main").unwrap();
    let unmerged_oid = commit_file(&wt_unmerged.path, "u.txt", "wip\n", "unmerged work");
    git(
        &fx.repo,
        &[
            "update-ref",
            "refs/aaf/snapshots/t-unmerged/s1",
            &unmerged_oid,
        ],
    );

    // Task merged: Done with NO MergeResult.head_sha, merged into main -> MUST CLEAN.
    let wt_merged = worktree::create(&fx.repo, &fx.worktrees, "t-merged", "main").unwrap();
    let merged_oid = commit_file(&wt_merged.path, "m.txt", "ok\n", "merged work");
    git(
        &fx.repo,
        &["update-ref", "refs/aaf/snapshots/t-merged/s1", &merged_oid],
    );
    git(
        &fx.repo,
        &["merge", "-q", "--no-ff", "aaf/t-merged", "-m", "merge m"],
    );

    let done_ids = vec!["t-unmerged".to_string(), "t-merged".to_string()];
    let active: HashSet<String> = done_ids.iter().cloned().collect();

    let report =
        clean_repo_offline_unlocked(&fx.repo, &data_dir, &fx.worktrees, &done_ids, &active, None);

    assert!(wt_unmerged.path.is_dir());
    assert!(has_branch(&fx.repo, "aaf/t-unmerged"));
    assert_eq!(
        git(&fx.repo, &["rev-parse", "refs/heads/aaf/t-unmerged"]),
        unmerged_oid
    );
    assert!(has_ref(&fx.repo, "refs/aaf/snapshots/t-unmerged/s1"));
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("t-unmerged") && w.contains("unmerged")),
        "expected unmerged warning, got {:?}",
        report.warnings
    );

    assert!(!wt_merged.path.exists());
    assert!(!has_branch(&fx.repo, "aaf/t-merged"));
    assert!(!has_ref(&fx.repo, "refs/aaf/snapshots/t-merged/s1"));

    // Non-`--force` `run_gc` must also refuse to delete `t-unmerged` when DB has no MergeResult.
    let db = Db::open(&data_dir.join("aaf.db")).unwrap();
    insert_done(&db, &fx.repo, "t-unmerged");
    drop(db);

    let gc_report = run_gc(
        &data_dir,
        &fx.worktrees,
        &GcOptions {
            repo_root: Some(fx.repo.clone()),
            ..GcOptions::default()
        },
    )
    .unwrap();
    assert!(wt_unmerged.path.is_dir());
    assert!(has_branch(&fx.repo, "aaf/t-unmerged"));
    assert!(
        gc_report
            .warnings
            .iter()
            .any(|w| w.contains("t-unmerged") && w.contains("unmerged")),
        "expected unmerged warning from run_gc, got {:?}",
        gc_report.warnings
    );

    // Once MergeResult records `head_sha == unmerged_oid`, non-force `run_gc` cleans it cleanly.
    let db = Db::open(&data_dir.join("aaf.db")).unwrap();
    db.emit(
        &FactoryEvent::new(
            EventKind::MergeResult,
            json!({ "merged": true, "kind": "squash_pr", "head_sha": unmerged_oid }).to_string(),
            1_700_000_010,
        )
        .for_task("t-unmerged"),
    )
    .unwrap();
    drop(db);

    let gc_clean = run_gc(
        &data_dir,
        &fx.worktrees,
        &GcOptions {
            repo_root: Some(fx.repo.clone()),
            ..GcOptions::default()
        },
    )
    .unwrap();
    assert!(gc_clean.warnings.is_empty(), "{:?}", gc_clean.warnings);
    assert!(!wt_unmerged.path.exists());
    assert!(!has_branch(&fx.repo, "aaf/t-unmerged"));
}

#[test]
fn f1_4_gc_respects_run_lock_held_under_linked_worktree_repo_id_and_sweeps_linked_batches() {
    let fx = Fixture::new();
    let root = fx.repo.parent().unwrap();
    let data_dir = root.join("data");
    fs::create_dir_all(&data_dir).unwrap();

    // Create a linked worktree simulating `aaf run` launched from a secondary worktree.
    let linked_wt = root.join("linked-wt");
    git(
        &fx.repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "dev-linked",
            linked_wt.to_str().unwrap(),
            "main",
        ],
    );
    let linked_wt = fs::canonicalize(&linked_wt).unwrap();
    let linked_repo_id = aaf_fp::repo_id(&linked_wt);
    assert_ne!(linked_repo_id, aaf_fp::repo_id(&fx.repo));

    // Stage a clean Done task + an orphan batch created under the linked worktree's repo_id.
    let db = Db::open(&data_dir.join("aaf.db")).unwrap();
    insert_done(&db, &fx.repo, "t-done-clean");
    drop(db);
    let wt_done = worktree::create(&fx.repo, &fx.worktrees, "t-done-clean", "main").unwrap();
    let wt_linked_batch =
        worktree::create(&linked_wt, &fx.worktrees, "batch-linked1", "main").unwrap();
    assert!(
        wt_linked_batch
            .path
            .starts_with(fx.worktrees.join(&linked_repo_id)),
        "expected batch under linked repo_id dir: {}",
        wt_linked_batch.path.display()
    );

    // Hold `.run.lock` under the LINKED worktree's `repo_id` (not main's `repo_id`).
    let locks_dir = data_dir.join("locks");
    private::create_dir(&locks_dir).unwrap();
    let linked_lock_path = locks_dir.join(format!("{linked_repo_id}.run.lock"));
    let held = private::open_file(&linked_lock_path).unwrap();
    held.try_lock().unwrap();

    // 1. `run_gc` targeting main repo must see the linked lock and skip as busy.
    let busy_main = run_gc(
        &data_dir,
        &fx.worktrees,
        &GcOptions {
            repo_root: Some(fx.repo.clone()),
            ..GcOptions::default()
        },
    )
    .unwrap();
    assert_eq!(busy_main.skipped_busy.len(), 1, "{busy_main:?}");
    assert!(busy_main.repos_scanned.is_empty());
    assert!(
        wt_done.path.is_dir(),
        "must not clean while linked lock held"
    );
    assert!(
        wt_linked_batch.path.is_dir(),
        "must not sweep batch while linked lock held"
    );

    // 2. `run_gc` targeting the linked worktree (and `--dry-run`) must also skip as busy.
    let busy_dry = run_gc(
        &data_dir,
        &fx.worktrees,
        &GcOptions {
            repo_root: Some(linked_wt.clone()),
            dry_run: true,
            ..GcOptions::default()
        },
    )
    .unwrap();
    assert_eq!(busy_dry.skipped_busy.len(), 1, "{busy_dry:?}");
    assert!(busy_dry.repos_scanned.is_empty());

    // 3. After releasing the linked lock, `run_gc` cleans BOTH the Done task AND
    //    the orphan batch stored under `worktrees/<linked_repo_id>/batch-linked1`.
    drop(held);
    let cleaned = run_gc(
        &data_dir,
        &fx.worktrees,
        &GcOptions {
            repo_root: Some(fx.repo.clone()),
            ..GcOptions::default()
        },
    )
    .unwrap();
    assert!(cleaned.skipped_busy.is_empty(), "{cleaned:?}");
    assert_eq!(cleaned.repos_scanned.len(), 1);
    assert!(!wt_done.path.exists());
    assert!(!has_branch(&fx.repo, "aaf/t-done-clean"));
    assert!(
        !wt_linked_batch.path.exists() && !has_branch(&fx.repo, "aaf/batch-linked1"),
        "orphan batch created from linked worktree must be swept: {cleaned:?}"
    );
    assert!(
        cleaned
            .removed_orphan_batches
            .contains(&"batch-linked1".to_string()),
        "{cleaned:?}"
    );
}
