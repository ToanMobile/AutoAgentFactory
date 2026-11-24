//! An agent can create a repository of its own inside its worktree
//! (`sub/.git`, config included). `git status` in the worktree runs a
//! `git status` inside it, which would start the program its
//! `core.fsmonitor` names, outside the agent's sandbox.

use std::fs;

use aaf_domain::ports::PortError;

use super::git_real::{Fixture, git};
use crate::factory::ports::GitOps;
use crate::throttle::tests::exec_support::write_executable;

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
    write_executable(
        &monitor,
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
    );
    git(
        &sub,
        &["config", "core.fsmonitor", monitor.to_str().unwrap()],
    );
    fs::write(tree.path.join("app.txt"), "two\n").unwrap();

    let committed = fx.ops.commit_all(&tree, "aaf: t1");

    assert!(!marker.exists(), "the embedded repository's fsmonitor ran");
    // O12-R10-3 (d): the nested repository is refused (it would land as a
    // bare gitlink), and it is the agent's to fix.
    assert!(
        matches!(committed, Err(PortError::Conflict(_))),
        "{committed:?}"
    );
    assert_eq!(git(&tree.path, &["rev-list", "--count", "main..HEAD"]), "0");
}

/// R5-L5-1 / O12-R10-3 (d): an embedded repository is never committed as a
/// gitlink nobody declared in `.gitmodules`: it is unstaged, nothing is
/// committed, and the error tells the agent what to do about the directory
/// instead of blaming a commit hook.
#[test]
fn an_undeclared_embedded_repository_is_unstaged_and_handed_back() {
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

    let error = fx.ops.commit_all(&tree, "aaf: t1").unwrap_err();

    assert!(matches!(error, PortError::Conflict(_)), "{error}");
    let error = error.to_string();
    assert!(error.contains("sub"), "{error}");
    assert!(error.contains("nested"), "{error}");
    assert!(!error.contains("commit hook"), "{error}");
    let after: u32 = count().parse().unwrap();
    assert_eq!(after, before, "nothing committed");
    let staged = git(&tree.path, &["diff", "--cached", "--raw"]);
    assert!(
        !staged.contains("160000"),
        "the gitlink stays unstaged: {staged}"
    );
}
