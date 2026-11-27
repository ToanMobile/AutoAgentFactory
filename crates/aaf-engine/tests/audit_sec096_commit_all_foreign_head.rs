#![allow(clippy::unwrap_used, clippy::expect_used)]
//! SEC-096 verification: `RealGit::commit_all` must refuse to commit when a
//! task worktree's `HEAD` no longer points at `refs/heads/<worktree.branch>`.
//!
//! A sandboxed agent can write `<common>/worktrees/<own>/HEAD` (e.g. via
//! `git symbolic-ref HEAD refs/heads/main`). When `aaf` subsequently runs
//! `commit_all` outside the sandbox, git must NOT advance `refs/heads/main`
//! without going through the merge gate.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use aaf_engine::factory::{BaseRef, GitOps, RealGit, resolve_base};
use tempfile::TempDir;

const SCRUBBED_GIT_ENV: [&str; 6] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_PREFIX",
];

fn run_git(dir: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    for var in SCRUBBED_GIT_ENV {
        cmd.env_remove(var);
    }
    let out = cmd.output().expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed (status={:?}):\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn write_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

struct Fixture {
    _tmp: TempDir,
    repo: PathBuf,
    git: RealGit,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "--quiet", "-b", "main"]);
        for (key, value) in [
            ("user.name", "aaf sec096 test"),
            ("user.email", "sec096@example.invalid"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", "/dev/null"),
        ] {
            run_git(&repo, &["config", key, value]);
        }

        write_file(&repo.join("README.md"), "base commit on main\n");
        run_git(&repo, &["add", "-A"]);
        run_git(&repo, &["commit", "--quiet", "-m", "initial commit"]);

        let base = resolve_base(&repo, "auto").unwrap();
        assert_eq!(
            base,
            BaseRef {
                remote: None,
                branch: "main".to_string(),
            }
        );

        let git = RealGit::new(
            repo.clone(),
            tmp.path().join("worktrees"),
            tmp.path().join("locks"),
            base,
        );

        Self {
            _tmp: tmp,
            repo,
            git,
        }
    }
}

#[test]
fn commit_all_refuses_when_worktree_head_points_at_main() {
    let fx = Fixture::new();
    let wt = fx.git.task_worktree("sec096-foreign").unwrap();
    assert_eq!(wt.branch, "aaf/sec096-foreign");

    let main_before = run_git(&fx.repo, &["rev-parse", "--verify", "refs/heads/main"]);

    // Simulate a sandboxed agent retargeting its own worktree HEAD to `main`.
    run_git(&wt.path, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    assert_eq!(
        run_git(&wt.path, &["symbolic-ref", "HEAD"]),
        "refs/heads/main"
    );

    write_file(&wt.path.join("hijacked.txt"), "unreviewed write to main\n");

    let res = fx.git.commit_all(&wt, "auto-commit after agent exit");
    let main_after = run_git(&fx.repo, &["rev-parse", "--verify", "refs/heads/main"]);

    assert!(
        res.is_err(),
        "SEC-096: commit_all must return Err when worktree HEAD points at refs/heads/main \
         instead of refs/heads/{}; got Ok(()) and refs/heads/main moved {} -> {}",
        wt.branch,
        main_before,
        main_after,
    );
    assert_eq!(
        main_after, main_before,
        "SEC-096: refs/heads/main must not advance when worktree HEAD was repointed to main"
    );
}

#[test]
fn commit_all_on_own_branch_still_commits() {
    let fx = Fixture::new();
    let wt = fx.git.task_worktree("sec096-own").unwrap();
    assert_eq!(wt.branch, "aaf/sec096-own");

    let branch_ref = format!("refs/heads/{}", wt.branch);
    let main_before = run_git(&fx.repo, &["rev-parse", "--verify", "refs/heads/main"]);
    let branch_before = run_git(&fx.repo, &["rev-parse", "--verify", &branch_ref]);

    write_file(&wt.path.join("work.txt"), "legitimate task change\n");

    let res = fx.git.commit_all(&wt, "legitimate task commit");
    assert!(
        res.is_ok(),
        "commit_all on own task branch should succeed; got: {:?}",
        res.as_ref().err(),
    );

    let main_after = run_git(&fx.repo, &["rev-parse", "--verify", "refs/heads/main"]);
    let branch_after = run_git(&fx.repo, &["rev-parse", "--verify", &branch_ref]);
    let commit_count = run_git(
        &fx.repo,
        &[
            "rev-list",
            "--count",
            &format!("{branch_before}..{branch_after}"),
        ],
    );

    assert_eq!(
        commit_count, "1",
        "task branch {} should advance by exactly 1 commit ({} -> {})",
        wt.branch, branch_before, branch_after,
    );
    assert_eq!(
        main_after, main_before,
        "refs/heads/main must remain unchanged when committing on task branch"
    );
}
