//! An agent can create a repository of its own inside its worktree
//! (`sub/.git`, config included). `git status` in the worktree runs a
//! `git status` inside it, which would start the program its
//! `core.fsmonitor` names, outside the agent's sandbox.

use std::fs;
use std::os::unix::fs::PermissionsExt;

use super::git_real::{Fixture, git};
use crate::factory::ports::GitOps;

#[test]
fn committing_a_worktree_runs_no_fsmonitor_of_an_embedded_repository() {
    let fx = Fixture::new();
    let tree = fx.ops.task_worktree("t1").unwrap();
    let sub = tree.path.join("sub");
    fs::create_dir_all(&sub).unwrap();
    git(&sub, &["init", "--quiet"]);
    fs::write(sub.join("y.txt"), "y\n").unwrap();
    git(&sub, &["add", "-A"]);
    git(
        &sub,
        &[
            "-c",
            "user.name=agent",
            "-c",
            "user.email=agent@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "sub",
        ],
    );
    let marker = fx.tmp.path().join("fsmonitor-ran");
    let monitor = fx.tmp.path().join("monitor.sh");
    fs::write(
        &monitor,
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&monitor, fs::Permissions::from_mode(0o755)).unwrap();
    git(
        &sub,
        &["config", "core.fsmonitor", monitor.to_str().unwrap()],
    );
    fs::write(tree.path.join("app.txt"), "two\n").unwrap();

    let committed = fx.ops.commit_all(&tree, "aaf: t1");
    let rebased = fx.ops.rebase(&tree);

    assert!(!marker.exists(), "the embedded repository's fsmonitor ran");
    committed.unwrap();
    assert!(rebased.unwrap().is_empty());
}

/// R5-L5-1: with no hook at all, what stays dirty after the commit is what
/// `git add` cannot stage (here an embedded repository's untracked file).
/// The error says so instead of blaming a commit hook and a follow-up
/// commit that never ran.
#[test]
fn unstageable_embedded_changes_are_not_blamed_on_a_commit_hook() {
    let fx = Fixture::new();
    let tree = fx.ops.task_worktree("t1").unwrap();
    let sub = tree.path.join("sub");
    fs::create_dir_all(&sub).unwrap();
    git(&sub, &["init", "--quiet"]);
    fs::write(sub.join("y.txt"), "y\n").unwrap();
    git(&sub, &["add", "-A"]);
    git(
        &sub,
        &[
            "-c",
            "user.name=agent",
            "-c",
            "user.email=agent@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "sub",
        ],
    );
    fs::write(sub.join("untracked.txt"), "u\n").unwrap();
    fs::write(tree.path.join("app.txt"), "two\n").unwrap();
    let count = || git(&tree.path, &["rev-list", "--count", "HEAD"]);
    let before: u32 = count().parse().unwrap();

    let error = fx.ops.commit_all(&tree, "aaf: t1").unwrap_err().to_string();

    assert!(error.contains("sub"), "{error}");
    assert!(error.contains("cannot stage"), "{error}");
    assert!(!error.contains("commit hook"), "{error}");
    assert!(!error.contains("follow-up commit"), "{error}");
    let after: u32 = count().parse().unwrap();
    assert_eq!(after, before + 1, "exactly one new commit");
}
