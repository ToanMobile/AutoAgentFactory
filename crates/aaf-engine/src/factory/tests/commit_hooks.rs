//! `RealGit::commit_all` and the repository's own commit hooks: a hook may
//! wait for a build slot far longer than an ordinary git call, and a hook
//! that rewrites files after the commit gets them committed once more.
//! Real `git`, real hook scripts.

use std::fs;
use std::time::Duration;

use super::git_real::{Fixture, git};
use crate::factory::RealGit;
use crate::factory::git_real::{COMMIT_LOCK_TIMEOUT, COMMIT_TIMEOUT};
use crate::factory::ports::GitOps;
use crate::factory::resolve_base;
use crate::git::cmd::GIT_TIMEOUT;
use crate::throttle::tests::exec_support::write_executable;

/// Makes `script` the `name` hook of the repository and all its worktrees.
fn install_hook(fx: &Fixture, name: &str, script: &str) {
    let hooks = fx.tmp.path().join("hooks");
    fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join(name);
    write_executable(&hook, script);
    git(
        &fx.repo,
        &["config", "core.hooksPath", hooks.to_str().unwrap()],
    );
}

fn real_git(fx: &Fixture) -> RealGit {
    RealGit::new(
        fx.repo.clone(),
        fx.tmp.path().join("worktrees"),
        fx.tmp.path().join("data/locks"),
        resolve_base(&fx.repo, "auto").unwrap(),
    )
}

/// The README's pre-commit `aaf slot -- ./gradlew lint` may first wait for a
/// heavy slot, and that wait has no ceiling of its own (only waiting for CPU
/// load ends after `heavy_wait_secs`): a commit is not an ordinary git call.
#[test]
fn a_commit_has_a_budget_of_its_own_longer_than_any_git_call() {
    assert!(COMMIT_TIMEOUT > GIT_TIMEOUT, "{COMMIT_TIMEOUT:?}");
    assert!(
        COMMIT_TIMEOUT >= Duration::from_secs(1800),
        "{COMMIT_TIMEOUT:?}"
    );
    // A waiter outlasts a holder that is inside one commit.
    assert!(
        COMMIT_LOCK_TIMEOUT >= COMMIT_TIMEOUT,
        "{COMMIT_LOCK_TIMEOUT:?} < {COMMIT_TIMEOUT:?}"
    );
}

#[test]
fn a_pre_commit_hook_is_bypassed_under_plan_v3_headless_commits() {
    let fx = Fixture::new();
    install_hook(&fx, "pre-commit", "#!/bin/sh\nsleep 2\nexit 1\n");
    let tree = fx.ops.task_worktree("t1").unwrap();
    fs::write(tree.path.join("app.txt"), "two\n").unwrap();
    let impatient = real_git(&fx).with_commit_timeout(Duration::from_secs(1));

    impatient.commit_all(&tree, "aaf: t1").unwrap();

    assert_eq!(git(&tree.path, &["status", "--porcelain"]), "");
    assert_eq!(git(&tree.path, &["rev-list", "--count", "main..HEAD"]), "1");
}

#[test]
fn a_slow_pre_commit_hook_commits_with_the_default_budget() {
    let fx = Fixture::new();
    install_hook(&fx, "pre-commit", "#!/bin/sh\nsleep 2\n");
    let tree = fx.ops.task_worktree("t1").unwrap();
    fs::write(tree.path.join("app.txt"), "two\n").unwrap();

    fx.ops.commit_all(&tree, "aaf: t1").unwrap();

    assert_eq!(git(&tree.path, &["status", "--porcelain"]), "");
    assert_eq!(git(&tree.path, &["rev-list", "--count", "main..HEAD"]), "1");
}

/// Plan v3 bypasses repo commit hooks (`-c core.hooksPath=/dev/null --no-verify`),
/// so a mutating post-commit hook never dirties the worktree or requires follow-up commits.
#[test]
fn a_post_commit_hook_never_dirties_the_worktree_under_plan_v3() {
    let fx = Fixture::new();
    install_hook(
        &fx,
        "post-commit",
        "#!/bin/sh\nn=$(cat stamp 2>/dev/null || echo 0)\necho $((n + 1)) > stamp\n",
    );
    let tree = fx.ops.task_worktree("t1").unwrap();
    fs::write(tree.path.join("app.txt"), "two\n").unwrap();

    fx.ops.commit_all(&tree, "aaf: t1").unwrap();

    assert!(!tree.path.join("stamp").exists());
    assert_eq!(git(&tree.path, &["status", "--porcelain"]), "");
    assert_eq!(git(&tree.path, &["rev-list", "--count", "main..HEAD"]), "1");
}
