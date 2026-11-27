#![allow(clippy::unwrap_used, clippy::expect_used)]
//! SEC-125 verification: host git operations (`RealGit::rebase`, `RealGit::reset_to`)
//! must fail closed when a task worktree's `ORIG_HEAD` is a symref (`ref: ...`),
//! instead of letting git update a protected/foreign branch ref.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use aaf_engine::factory::{BaseRef, GitOps, RealGit, resolve_base};
use aaf_engine::git::TaskWorktree;
use tempfile::TempDir;

const SCRUBBED_GIT_ENV: [&str; 6] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_PREFIX",
];

fn git_cmd(dir: &Path, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    for var in SCRUBBED_GIT_ENV {
        cmd.env_remove(var);
    }
    cmd.output().expect("spawn git")
}

fn run_git(dir: &Path, args: &[&str]) -> String {
    let out = git_cmd(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed (status={:?}):\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn try_rev_parse(dir: &Path, rev: &str) -> Option<String> {
    let out = git_cmd(dir, &["rev-parse", "--verify", "--quiet", rev]);
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
}

fn git_dir_abs(wt_path: &Path) -> PathBuf {
    let raw = PathBuf::from(run_git(wt_path, &["rev-parse", "--git-dir"]));
    if raw.is_absolute() {
        raw
    } else {
        wt_path.join(raw)
    }
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
            ("user.name", "aaf sec125 test"),
            ("user.email", "sec125@example.invalid"),
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

    fn commit_on_main(&self, rel: &str, content: &str, msg: &str) -> String {
        write_file(&self.repo.join(rel), content);
        run_git(&self.repo, &["add", "-A"]);
        run_git(&self.repo, &["commit", "--quiet", "-m", msg]);
        run_git(&self.repo, &["rev-parse", "--verify", "refs/heads/main"])
    }

    fn task_with_commit(&self, task_id: &str) -> (TaskWorktree, String, String) {
        let wt = self.git.task_worktree(task_id).unwrap();
        let parent_sha = run_git(&wt.path, &["rev-parse", "--verify", "HEAD"]);
        write_file(&wt.path.join("task_work.txt"), "unreviewed agent commit\n");
        self.git.commit_all(&wt, "agent commit").unwrap();
        let task_sha = run_git(&wt.path, &["rev-parse", "--verify", "HEAD"]);
        assert_ne!(task_sha, parent_sha);
        (wt, parent_sha, task_sha)
    }
}

#[test]
fn rebase_refuses_symref_orig_head() {
    let fx = Fixture::new();
    let (wt, _, task_sha) = fx.task_with_commit("sec125-rebase-symref");
    let main_before = fx.commit_on_main("main_only.txt", "advance main\n", "advance main");

    let git_dir = git_dir_abs(&wt.path);
    write_file(&git_dir.join("ORIG_HEAD"), "ref: refs/heads/main\n");

    let res = fx.git.rebase(&wt);
    let main_after = run_git(&fx.repo, &["rev-parse", "--verify", "refs/heads/main"]);

    assert!(
        res.is_err(),
        "SEC-125: rebase must fail closed when ORIG_HEAD is a symref to refs/heads/main; \
         got {res:?}, main moved {main_before} -> {main_after} (task_sha={task_sha})"
    );
    assert_eq!(
        main_after, main_before,
        "SEC-125: refs/heads/main must not move when ORIG_HEAD is a symref"
    );
}

#[test]
fn reset_to_refuses_symref_orig_head() {
    let fx = Fixture::new();
    let (wt, parent_sha, task_sha) = fx.task_with_commit("sec125-reset-symref");
    assert_eq!(try_rev_parse(&fx.repo, "refs/heads/develop"), None);

    let git_dir = git_dir_abs(&wt.path);
    write_file(&git_dir.join("ORIG_HEAD"), "ref: refs/heads/develop\n");

    let res = fx.git.reset_to(&wt, &parent_sha);
    let develop_after = try_rev_parse(&fx.repo, "refs/heads/develop");

    assert!(
        res.is_err(),
        "SEC-125: reset_to must fail closed when ORIG_HEAD is a symref to refs/heads/develop; \
         got {res:?}, refs/heads/develop={develop_after:?} (task_sha={task_sha})"
    );
    assert_eq!(
        develop_after, None,
        "SEC-125: refs/heads/develop must not be created when ORIG_HEAD is a symref"
    );
}

#[test]
fn rebase_with_plain_orig_head_still_works() {
    let fx = Fixture::new();
    let (wt, _, _) = fx.task_with_commit("sec125-rebase-plain");
    let main_before = fx.commit_on_main("main_only.txt", "advance main\n", "advance main");

    let res = fx.git.rebase(&wt);
    assert!(
        res.is_ok(),
        "rebase with untouched ORIG_HEAD should succeed; got: {:?}",
        res.as_ref().err()
    );
    assert!(
        res.unwrap().is_empty(),
        "clean rebase should report no conflicted files"
    );

    let main_after = run_git(&fx.repo, &["rev-parse", "--verify", "refs/heads/main"]);
    assert_eq!(
        main_after, main_before,
        "refs/heads/main must remain unchanged after task worktree rebase"
    );
}
