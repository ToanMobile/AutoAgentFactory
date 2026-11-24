//! `RealGit::commit_all` runs one `git commit` per repository at a time, so
//! `--max-parallel N` never fires N pre-commit hooks at once. Real `git`,
//! real hook scripts, real lock files in a temp dir.

use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::Barrier;
use std::time::Duration;

use super::git_real::{Fixture, git};
use crate::factory::ports::GitOps;
use crate::factory::{RealGit, resolve_base};
use crate::git::TaskWorktree;
use crate::throttle::tests::exec_support::write_executable;

fn commit_lock(fx: &Fixture) -> PathBuf {
    let name = format!("{}.commit.lock", aaf_fp::repo_id(&fx.repo));
    fx.tmp.path().join("data/locks").join(name)
}

/// Makes `script` the pre-commit hook of the repository and all its worktrees.
fn install_hook(fx: &Fixture, script: &str) {
    let hooks = fx.tmp.path().join("hooks");
    fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("pre-commit");
    write_executable(&hook, script);
    git(
        &fx.repo,
        &["config", "core.hooksPath", hooks.to_str().unwrap()],
    );
}

/// A task worktree with one change nobody committed yet.
fn uncommitted(fx: &Fixture, id: &str) -> TaskWorktree {
    let tree = fx.ops.task_worktree(id).unwrap();
    fs::write(tree.path.join("work.txt"), id).unwrap();
    tree
}

fn held(lock: &Path) -> bool {
    let Ok(file) = File::options().write(true).open(lock) else {
        return false;
    };
    match file.try_lock() {
        Ok(()) => {
            file.unlock().unwrap();
            false
        }
        Err(TryLockError::WouldBlock) => true,
        Err(TryLockError::Error(error)) => panic!("{error}"),
    }
}

#[test]
fn four_concurrent_commits_bypass_untrusted_pre_commit_hooks_and_commit_cleanly() {
    let fx = Fixture::new();
    let trees: Vec<TaskWorktree> = ["c1", "c2", "c3", "c4"]
        .iter()
        .map(|id| uncommitted(&fx, id))
        .collect();
    let dir = fx.tmp.path().join("hook-state");
    fs::create_dir_all(&dir).unwrap();
    // Under Plan v3 (V2-SEC-03), commit_all passes `--no-verify` so untrusted
    // repo pre-commit hooks never execute outside the sandbox before head_lock.
    install_hook(
        &fx,
        &format!("#!/bin/sh\necho ran >> '{}/log'\nexit 1\n", dir.display()),
    );
    let lock = commit_lock(&fx);
    let start = Barrier::new(trees.len());
    let results = std::thread::scope(|scope| {
        let handles: Vec<_> = trees
            .iter()
            .map(|tree| {
                let (fx, start) = (&fx, &start);
                scope.spawn(move || {
                    start.wait();
                    fx.ops.commit_all(tree, "aaf: concurrent")
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    for result in results {
        result.unwrap();
    }
    assert!(
        !dir.join("log").exists(),
        "commit_all must not execute untrusted pre-commit hooks"
    );
    for tree in &trees {
        assert_eq!(git(&tree.path, &["status", "--porcelain"]), "");
        assert_eq!(git(&tree.path, &["rev-list", "--count", "main..HEAD"]), "1");
    }
    assert!(!held(&lock), "released with the commits");
}

#[test]
fn a_commit_stays_out_while_another_process_holds_the_commit_lock_of_the_repository() {
    let fx = Fixture::new();
    let tree = uncommitted(&fx, "c1");
    let log = fx.tmp.path().join("hook.log");
    install_hook(
        &fx,
        &format!("#!/bin/sh\necho start >> '{}'\n", log.display()),
    );
    // An `aaf run` of another process would hold exactly this lock.
    let lock = commit_lock(&fx);
    fs::create_dir_all(lock.parent().unwrap()).unwrap();
    let other = File::create(&lock).unwrap();
    other.try_lock().unwrap();
    let impatient = RealGit::new(
        fx.repo.clone(),
        fx.tmp.path().join("worktrees"),
        fx.tmp.path().join("data/locks"),
        resolve_base(&fx.repo, "auto").unwrap(),
    )
    .with_commit_lock_timeout(Duration::ZERO);

    let refused = impatient.commit_all(&tree, "aaf: c1").unwrap_err();

    assert!(refused.to_string().contains("commit.lock"), "{refused}");
    assert!(!log.exists(), "the hook ran beside the other process");
    assert_eq!(git(&tree.path, &["rev-list", "--count", "main..HEAD"]), "0");

    other.unlock().unwrap();
    impatient.commit_all(&tree, "aaf: c1").unwrap();
    assert!(!log.exists(), "commit_all passes --no-verify");
    assert_eq!(git(&tree.path, &["rev-list", "--count", "main..HEAD"]), "1");
    // Another repository commits under a lock of its own.
    assert!(!held(&lock), "released with the commit");
}
