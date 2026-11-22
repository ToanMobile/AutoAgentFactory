#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "git_support.rs"]
mod support;

use std::fs;

use aaf_engine::git::GitError;
use aaf_engine::git::worktree::{self, TaskWorktree};
use support::{Fixture, commit_file, git};

#[test]
fn create_makes_the_worktree_on_its_own_branch() {
    let fx = Fixture::new();
    let wt = worktree::create(&fx.repo, &fx.worktrees, "T-1", "main").unwrap();

    assert_eq!(wt.branch, "aaf/T-1");
    assert_eq!(
        wt.path,
        fx.worktrees.join(aaf_fp::repo_id(&fx.repo)).join("T-1")
    );
    assert_eq!(wt.task_id().unwrap(), "T-1");
    assert!(wt.path.join("README.md").is_file());
    assert_eq!(
        git(&wt.path, &["symbolic-ref", "--short", "HEAD"]),
        "aaf/T-1"
    );
    assert_eq!(fx.head(&wt.path, "HEAD"), fx.head(&fx.repo, "main"));
}

#[test]
fn create_twice_is_an_error_not_a_reuse() {
    let fx = Fixture::new();
    let wt = worktree::create(&fx.repo, &fx.worktrees, "t1", "main").unwrap();
    fs::write(wt.path.join("wip.txt"), "work").unwrap();

    let err = worktree::create(&fx.repo, &fx.worktrees, "t1", "main").unwrap_err();
    assert!(matches!(err, GitError::AlreadyExists(_)), "{err}");
    assert!(wt.path.join("wip.txt").is_file());
}

#[test]
fn create_refuses_a_leftover_branch_without_a_directory() {
    let fx = Fixture::new();
    git(&fx.repo, &["branch", "aaf/t1"]);
    let err = worktree::create(&fx.repo, &fx.worktrees, "t1", "main").unwrap_err();
    assert!(matches!(err, GitError::AlreadyExists(_)), "{err}");
    assert!(!fx.worktrees.exists());
}

#[test]
fn create_refuses_a_leftover_directory_without_a_branch() {
    let fx = Fixture::new();
    // Git itself would happily populate an existing empty directory.
    let leftover = worktree::worktree_path(&fx.repo, &fx.worktrees, "t1").unwrap();
    fs::create_dir_all(&leftover).unwrap();
    let err = worktree::create(&fx.repo, &fx.worktrees, "t1", "main").unwrap_err();
    assert!(matches!(err, GitError::AlreadyExists(_)), "{err}");
    assert_eq!(git(&fx.repo, &["branch", "--list", "aaf/t1"]), "");
}

#[test]
fn invalid_task_ids_never_reach_git_and_valid_ones_do() {
    let fx = Fixture::new();
    let too_long = "a".repeat(65);
    let blocked = [
        "",
        ".",
        "..",
        "../x",
        "a/b",
        "/abs",
        "a b",
        "a.b",
        "a\nb",
        "é",
        "a;b",
        "a$b",
        "a~1",
        "a:b",
        "a\\b",
        too_long.as_str(),
    ];
    for id in blocked {
        let err = worktree::create(&fx.repo, &fx.worktrees, id, "main").unwrap_err();
        assert!(matches!(err, GitError::InvalidTaskId(_)), "{id:?}: {err}");
        assert!(worktree::validate_task_id(id).is_err(), "{id:?}");
    }
    assert!(!fx.worktrees.exists(), "a rejected id created a directory");
    assert_eq!(git(&fx.repo, &["branch", "--list", "aaf/*"]), "");

    let longest = "b".repeat(64);
    for id in ["a", "A-1_z", "-x", "42", longest.as_str()] {
        let wt = worktree::create(&fx.repo, &fx.worktrees, id, "main").unwrap();
        assert!(wt.path.is_dir(), "{id:?}");
    }
}

#[test]
fn a_hand_built_worktree_with_a_foreign_branch_is_rejected() {
    let fx = Fixture::new();
    for branch in ["main", "aaf/../main", "aaf/", "--force"] {
        let wt = TaskWorktree {
            path: fx.repo.clone(),
            branch: branch.to_string(),
        };
        assert!(wt.task_id().is_err(), "{branch:?}");
        assert!(worktree::remove(&fx.repo, &wt, true, true).is_err());
    }
    assert!(fx.repo.join("README.md").is_file());
}

#[test]
fn create_rejects_a_base_that_is_not_a_commit_or_looks_like_an_option() {
    let fx = Fixture::new();
    for base in ["no-such-branch", "--detach", ""] {
        let err = worktree::create(&fx.repo, &fx.worktrees, "t1", base).unwrap_err();
        assert!(matches!(err, GitError::Invalid(_)), "{base:?}: {err}");
    }
    assert!(!fx.worktrees.exists());
}

#[test]
fn list_reports_the_main_tree_and_task_worktrees() {
    let fx = Fixture::new();
    let wt = worktree::create(&fx.repo, &fx.worktrees, "t1", "main").unwrap();

    let entries = worktree::list(&fx.repo).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].path, fx.repo);
    assert_eq!(entries[0].branch.as_deref(), Some("main"));
    assert_eq!(entries[0].task_id(), None);
    assert_eq!(entries[1].path, wt.path);
    assert_eq!(entries[1].branch.as_deref(), Some("aaf/t1"));
    assert_eq!(entries[1].task_id(), Some("t1"));
    assert_eq!(entries[1].head, Some(fx.head(&fx.repo, "main")));
}

#[test]
fn remove_deletes_a_clean_worktree_and_its_merged_branch() {
    let fx = Fixture::new();
    let wt = worktree::create(&fx.repo, &fx.worktrees, "t1", "main").unwrap();

    worktree::remove(&fx.repo, &wt, true, false).unwrap();
    assert!(!wt.path.exists());
    assert_eq!(git(&fx.repo, &["branch", "--list", "aaf/t1"]), "");
    assert_eq!(worktree::list(&fx.repo).unwrap().len(), 1);
}

#[test]
fn remove_keeps_the_branch_when_not_asked_to_delete_it() {
    let fx = Fixture::new();
    let wt = worktree::create(&fx.repo, &fx.worktrees, "t1", "main").unwrap();
    let tip = commit_file(&wt.path, "a.txt", "a\n", "task work");

    worktree::remove(&fx.repo, &wt, false, false).unwrap();
    assert!(!wt.path.exists());
    assert_eq!(fx.head(&fx.repo, "aaf/t1"), tip);
}

#[test]
fn remove_refuses_uncommitted_changes_unless_forced() {
    let fx = Fixture::new();
    for (id, file) in [("untracked", "new.txt"), ("modified", "README.md")] {
        let wt = worktree::create(&fx.repo, &fx.worktrees, id, "main").unwrap();
        fs::write(wt.path.join(file), "changed").unwrap();

        let err = worktree::remove(&fx.repo, &wt, true, false).unwrap_err();
        assert!(matches!(err, GitError::Dirty(_)), "{id}: {err}");
        assert!(wt.path.join(file).is_file(), "{id}: work was lost");

        worktree::remove(&fx.repo, &wt, true, true).unwrap();
        assert!(!wt.path.exists());
    }
    assert_eq!(git(&fx.repo, &["branch", "--list", "aaf/*"]), "");
}

#[test]
fn remove_refuses_to_delete_an_unmerged_branch_unless_forced() {
    let fx = Fixture::new();
    let wt = worktree::create(&fx.repo, &fx.worktrees, "t1", "main").unwrap();
    let tip = commit_file(&wt.path, "a.txt", "a\n", "task work");

    let err = worktree::remove(&fx.repo, &wt, true, false).unwrap_err();
    assert!(matches!(err, GitError::Unmerged(_)), "{err}");
    assert!(wt.path.is_dir(), "the worktree went before the refusal");
    assert_eq!(fx.head(&fx.repo, "aaf/t1"), tip);

    worktree::remove(&fx.repo, &wt, true, true).unwrap();
    assert_eq!(git(&fx.repo, &["branch", "--list", "aaf/t1"]), "");
}

#[test]
fn remove_refuses_a_path_that_is_not_the_branch_worktree() {
    let fx = Fixture::new();
    let wt = worktree::create(&fx.repo, &fx.worktrees, "t1", "main").unwrap();
    let other = worktree::create(&fx.repo, &fx.worktrees, "t2", "main").unwrap();
    let forged = TaskWorktree {
        path: other.path.clone(),
        branch: wt.branch.clone(),
    };
    let err = worktree::remove(&fx.repo, &forged, true, true).unwrap_err();
    assert!(matches!(err, GitError::Invalid(_)), "{err}");
    assert!(other.path.is_dir());
    assert_eq!(worktree::list(&fx.repo).unwrap().len(), 3);
}

#[test]
fn prune_forgets_a_worktree_whose_directory_is_gone() {
    let fx = Fixture::new();
    let wt = worktree::create(&fx.repo, &fx.worktrees, "t1", "main").unwrap();
    fs::remove_dir_all(&wt.path).unwrap();
    assert_eq!(worktree::list(&fx.repo).unwrap().len(), 2);

    worktree::prune(&fx.repo).unwrap();
    assert_eq!(worktree::list(&fx.repo).unwrap().len(), 1);
}
