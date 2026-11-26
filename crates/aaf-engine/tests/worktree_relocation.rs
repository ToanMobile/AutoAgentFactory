#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "git_support.rs"]
mod support;

use std::fs;
use std::path::Path;

use aaf_domain::ports::TaskStore;
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::factory::{GitOps, RealGit, resolve_base};
use aaf_engine::gc::{GcOptions, run_gc};
use aaf_engine::git::worktree;
use support::{Fixture, commit_file, git};

fn real_git(repo: &Path, worktrees: &Path, locks: &Path) -> RealGit {
    let base = resolve_base(repo, "main").unwrap();
    RealGit::new(
        repo.to_path_buf(),
        worktrees.to_path_buf(),
        locks.to_path_buf(),
        base,
    )
}

#[test]
fn task_worktree_reuses_existing_aaf_worktree_after_worktree_dir_change() {
    let fx = Fixture::new();
    let old_root = fx.worktrees.clone();
    let base_dir = fx.worktrees.parent().unwrap();
    let new_root = base_dir.join("new-worktrees");

    let old_git = real_git(&fx.repo, &old_root, &fx.locks);
    let wt1 = old_git.task_worktree("t-reloc").unwrap();
    let head = commit_file(&wt1.path, "feat.txt", "from-old-root\n", "work in old root");
    fs::write(wt1.path.join("wip.txt"), "uncommitted\n").unwrap();

    // Switch AAF_WORKTREE_DIR to a brand-new root (e.g. same-volume APFS root).
    let new_git = real_git(&fx.repo, &new_root, &fx.locks);
    let wt2 = new_git.task_worktree("t-reloc").unwrap();

    assert_eq!(wt2.branch, "aaf/t-reloc");
    assert_eq!(
        fs::canonicalize(&wt2.path).unwrap(),
        fs::canonicalize(&wt1.path).unwrap(),
        "must reuse the existing AAF worktree instead of failing with already exists: branch"
    );
    assert_eq!(fx.head(&wt2.path, "HEAD"), head);
    assert!(wt2.path.join("wip.txt").is_file(), "WIP must be preserved");

    // existing_registered and existing_head also find the relocated worktree.
    let (reg_wt, reg_head) = worktree::existing_registered(&fx.repo, &new_root, "t-reloc")
        .unwrap()
        .expect("existing_registered must find relocated AAF worktree");
    assert_eq!(
        fs::canonicalize(&reg_wt.path).unwrap(),
        fs::canonicalize(&wt1.path).unwrap()
    );
    assert_eq!(reg_head, head);

    // Once merged, cleanup from the new worktree root removes the old worktree and branch cleanly.
    fs::remove_file(wt2.path.join("wip.txt")).unwrap();
    git(&fx.repo, &["merge", "--ff-only", "aaf/t-reloc"]);
    new_git.cleanup("t-reloc").unwrap();
    assert!(!wt1.path.exists());
    assert_eq!(git(&fx.repo, &["branch", "--list", "aaf/t-reloc"]), "");
}

#[test]
fn task_worktree_reattaches_orphaned_branch_when_directory_was_deleted() {
    let fx = Fixture::new();
    let old_root = fx.worktrees.clone();
    let base_dir = fx.worktrees.parent().unwrap();
    let new_root = base_dir.join("new-worktrees");

    let old_git = real_git(&fx.repo, &old_root, &fx.locks);
    let wt1 = old_git.task_worktree("t-orphan").unwrap();
    let head = commit_file(&wt1.path, "saved.txt", "keep-me\n", "saved commit");

    // Simulate external directory deletion without pruning git worktree metadata.
    fs::remove_dir_all(&wt1.path).unwrap();
    assert!(
        worktree::existing_registered(&fx.repo, &new_root, "t-orphan")
            .unwrap()
            .is_none(),
        "existing_registered must stay None once the worktree directory is gone"
    );

    let new_git = real_git(&fx.repo, &new_root, &fx.locks);
    let wt2 = new_git.task_worktree("t-orphan").unwrap();
    assert_eq!(wt2.branch, "aaf/t-orphan");
    assert_eq!(
        wt2.path,
        worktree::worktree_path(&fx.repo, &new_root, "t-orphan").unwrap()
    );
    assert_eq!(fx.head(&wt2.path, "HEAD"), head);
    assert_eq!(
        fs::read_to_string(wt2.path.join("saved.txt")).unwrap(),
        "keep-me\n"
    );
}

#[test]
fn task_worktree_refuses_branch_checked_out_in_main_or_foreign_worktree() {
    let fx = Fixture::new();
    let base_dir = fx.worktrees.parent().unwrap();
    let real = real_git(&fx.repo, &fx.worktrees, &fx.locks);

    // Case 1: Human checked out aaf/t-foreign in a custom directory.
    let foreign_dir = base_dir.join("human-review");
    git(
        &fx.repo,
        &[
            "worktree",
            "add",
            "-b",
            "aaf/t-foreign",
            foreign_dir.to_str().unwrap(),
            "main",
        ],
    );
    assert!(
        worktree::existing_registered(&fx.repo, &fx.worktrees, "t-foreign")
            .unwrap()
            .is_none(),
        "foreign checkout must not be treated as an AAF worktree"
    );
    let err = real.task_worktree("t-foreign").unwrap_err();
    assert!(
        err.to_string().contains("already checked out"),
        "expected fail-closed foreign checkout error, got: {err}"
    );

    // Case 2: Human checked out aaf/t-nested inside the repo itself (even with matching suffix).
    let nested_dir = fx
        .repo
        .join(".claude/worktrees")
        .join(aaf_fp::repo_id(&fx.repo))
        .join("t-nested");
    git(
        &fx.repo,
        &[
            "worktree",
            "add",
            "-b",
            "aaf/t-nested",
            nested_dir.to_str().unwrap(),
            "main",
        ],
    );
    assert!(
        worktree::existing_registered(&fx.repo, &fx.worktrees, "t-nested")
            .unwrap()
            .is_none(),
        "worktree inside main repo must never be treated as an AAF worktree"
    );
    assert!(real.task_worktree("t-nested").is_err());

    // Case 3: Missing worktree directory that is git-worktree-locked must fail closed.
    let wt_locked = worktree::create(&fx.repo, &fx.worktrees, "t-locked", "main").unwrap();
    git(
        &fx.repo,
        &["worktree", "lock", wt_locked.path.to_str().unwrap()],
    );
    fs::remove_dir_all(&wt_locked.path).unwrap();
    let err = real.task_worktree("t-locked").unwrap_err();
    assert!(
        err.to_string().contains("locked"),
        "expected fail-closed locked worktree error, got: {err}"
    );
}

#[test]
fn cleanup_verified_merged_and_gc_handle_relocated_aaf_worktrees() {
    let fx = Fixture::new();
    let old_root = fx.worktrees.clone();
    let base_dir = fx.worktrees.parent().unwrap();
    let new_root = base_dir.join("new-worktrees");
    let data = base_dir.join("data");
    fs::create_dir_all(&data).unwrap();

    // Squash-merged cleanup across relocated worktree root.
    let old_git = real_git(&fx.repo, &old_root, &fx.locks);
    let wt_sq = old_git.task_worktree("t-sq").unwrap();
    let sq_head = commit_file(&wt_sq.path, "sq.txt", "sq\n", "sq work");
    let new_git = real_git(&fx.repo, &new_root, &fx.locks);
    new_git.cleanup_verified_merged("t-sq", &sq_head).unwrap();
    assert!(!wt_sq.path.exists());
    assert_eq!(git(&fx.repo, &["branch", "--list", "aaf/t-sq"]), "");

    // GC removes Done task worktree created under old_root when run with new_root.
    let wt_gc = old_git.task_worktree("t-gc").unwrap();
    let _ = commit_file(&wt_gc.path, "gc.txt", "gc\n", "gc work");
    git(&fx.repo, &["merge", "--ff-only", "aaf/t-gc"]);

    let db = Db::open(&data.join("aaf.db")).unwrap();
    let spec = ImmutableSpec {
        description: "desc".to_string(),
        acceptance: vec!["ok".to_string()],
    };
    let mut task = Task::new_root(
        "t-gc",
        fx.repo.to_string_lossy().into_owned(),
        "done relocated",
        spec,
        Priority::P2,
        1,
    );
    task.state = TaskState::Done;
    db.insert_task(&task).unwrap();
    drop(db);

    let report = run_gc(
        &data,
        &new_root,
        &GcOptions {
            repo_root: Some(fx.repo.clone()),
            ..GcOptions::default()
        },
    )
    .unwrap();
    assert!(
        report.warnings.is_empty(),
        "warnings: {:?}",
        report.warnings
    );
    assert!(
        !wt_gc.path.exists(),
        "relocated worktree should be removed by gc"
    );
    assert_eq!(git(&fx.repo, &["branch", "--list", "aaf/t-gc"]), "");
}

#[test]
fn reattach_blocks_untrusted_post_checkout_hook_and_setup_cmd() {
    let fx = Fixture::new();
    let base_dir = fx.worktrees.parent().unwrap();
    let new_root = base_dir.join("new-worktrees");
    let marker_hook = base_dir.join("hook-rce.marker");
    let marker_setup = base_dir.join("setup-rce.marker");

    // Base commit defines trusted [worktree] setup_cmd and repo-relative hooksPath.
    fs::create_dir_all(fx.repo.join(".autofactory")).unwrap();
    fs::write(
        fx.repo.join(".autofactory/config.toml"),
        "[worktree]\nsetup_cmd = \"sh ./setup.sh\"\n",
    )
    .unwrap();
    fs::write(fx.repo.join("setup.sh"), "#!/bin/sh\nexit 0\n").unwrap();
    fs::create_dir_all(fx.repo.join(".githooks")).unwrap();
    fs::write(
        fx.repo.join(".githooks/post-checkout"),
        "#!/bin/sh\nexit 0\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            fx.repo.join(".githooks/post-checkout"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    git(&fx.repo, &["add", "."]);
    git(&fx.repo, &["commit", "-q", "-m", "add setup and hooks"]);
    git(&fx.repo, &["config", "core.hooksPath", ".githooks"]);

    let old_git = real_git(&fx.repo, &fx.worktrees, &fx.locks);
    let wt1 = old_git.task_worktree("t-rce").unwrap();
    // Untrusted agent modifies setup.sh and .githooks/post-checkout on the task branch.
    fs::write(
        wt1.path.join("setup.sh"),
        format!("#!/bin/sh\necho pwn > '{}'\n", marker_setup.display()),
    )
    .unwrap();
    fs::write(
        wt1.path.join(".githooks/post-checkout"),
        format!("#!/bin/sh\necho pwn > '{}'\n", marker_hook.display()),
    )
    .unwrap();
    git(&wt1.path, &["add", "."]);
    git(&wt1.path, &["commit", "-q", "-m", "untrusted payload"]);

    // Delete worktree dir and reattach under new_root.
    fs::remove_dir_all(&wt1.path).unwrap();
    let new_git = real_git(&fx.repo, &new_root, &fx.locks);
    let wt2 = new_git.task_worktree("t-rce").unwrap();
    assert!(wt2.path.is_dir());
    assert!(
        !marker_hook.exists(),
        "post-checkout hook on untrusted branch must NOT execute on reattach"
    );
    assert!(
        !marker_setup.exists(),
        "setup_cmd on untrusted branch must NOT execute on reattach"
    );
}

#[cfg(unix)]
#[test]
fn symlink_at_expected_worktree_cannot_adopt_main_or_foreign_checkout() {
    use std::os::unix::fs::symlink;

    let fx = Fixture::new();
    let base_dir = fx.worktrees.parent().unwrap();
    let foreign = base_dir.join("foreign-human");
    git(
        &fx.repo,
        &[
            "worktree",
            "add",
            "-b",
            "aaf/t-sym",
            foreign.to_str().unwrap(),
            "main",
        ],
    );
    let expected = worktree::worktree_path(&fx.repo, &fx.worktrees, "t-sym").unwrap();
    fs::create_dir_all(expected.parent().unwrap()).unwrap();
    symlink(&foreign, &expected).unwrap();

    assert!(
        worktree::existing_registered(&fx.repo, &fx.worktrees, "t-sym")
            .unwrap()
            .is_none(),
        "symlink at expected must never adopt a foreign checkout"
    );
    let head = fx.head(&foreign, "HEAD");
    let real = real_git(&fx.repo, &fx.worktrees, &fx.locks);
    assert!(
        real.cleanup_verified_merged("t-sym", &head).is_err(),
        "cleanup_verified_merged must refuse symlink at expected"
    );
    assert!(foreign.is_dir(), "foreign checkout must remain untouched");
}

#[test]
fn detached_relocated_worktree_is_cleaned_when_clean_and_kept_when_dirty() {
    let fx = Fixture::new();
    let old_root = fx.worktrees.clone();
    let base_dir = fx.worktrees.parent().unwrap();
    let new_root = base_dir.join("new-worktrees");
    let data = base_dir.join("data");
    fs::create_dir_all(&data).unwrap();

    let old_git = real_git(&fx.repo, &old_root, &fx.locks);
    let wt_dirty = old_git.task_worktree("t-det-dirty").unwrap();
    let _ = commit_file(&wt_dirty.path, "d.txt", "d\n", "dirty work");
    git(&fx.repo, &["merge", "--ff-only", "aaf/t-det-dirty"]);
    git(&wt_dirty.path, &["checkout", "--detach", "HEAD"]);
    fs::write(wt_dirty.path.join("wip.txt"), "keep\n").unwrap();

    let db = Db::open(&data.join("aaf.db")).unwrap();
    let spec = ImmutableSpec {
        description: "desc".to_string(),
        acceptance: vec!["ok".to_string()],
    };
    let mut task = Task::new_root(
        "t-det-dirty",
        fx.repo.to_string_lossy().into_owned(),
        "detached dirty",
        spec,
        Priority::P2,
        1,
    );
    task.state = TaskState::Done;
    db.insert_task(&task).unwrap();
    drop(db);

    let report = run_gc(
        &data,
        &new_root,
        &GcOptions {
            repo_root: Some(fx.repo.clone()),
            ..GcOptions::default()
        },
    )
    .unwrap();
    assert!(
        report.skipped_dirty.contains(&"t-det-dirty".to_string()),
        "detached dirty relocated worktree must be skipped: {report:?}"
    );
    assert!(wt_dirty.path.join("wip.txt").is_file());
    assert_eq!(
        git(&fx.repo, &["branch", "--list", "aaf/t-det-dirty"]).trim(),
        "aaf/t-det-dirty",
        "branch must not be deleted while detached relocated worktree is dirty"
    );
}
