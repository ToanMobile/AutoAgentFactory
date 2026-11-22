//! Shared fixture for the `git_*` integration tests (included with
//! `#[path]`; as a test target of its own it holds no tests).
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::new_without_default
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// Runs git for test setup and returns trimmed stdout; panics on failure.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Repo-local settings so the developer's global git config (identity,
/// signing, hooks) cannot change what the tests observe.
fn isolate(repo: &Path) {
    for (key, value) in [
        ("user.name", "aaf test"),
        ("user.email", "aaf@example.invalid"),
        ("commit.gpgsign", "false"),
        ("core.hooksPath", "/dev/null"),
    ] {
        git(repo, &["config", key, value]);
    }
}

pub struct Fixture {
    _tmp: TempDir,
    /// Main work tree, on branch `main` with one commit.
    pub repo: PathBuf,
    pub worktrees: PathBuf,
    pub locks: PathBuf,
    /// Bare repository registered as `origin`, when asked for.
    pub remote: Option<PathBuf>,
}

impl Fixture {
    pub fn new() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        // macOS temp dirs sit behind a symlink; git reports real paths.
        let root = fs::canonicalize(tmp.path()).unwrap();
        let repo = root.join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        isolate(&repo);
        commit_file(&repo, "README.md", "hello\n", "init");
        Fixture {
            _tmp: tmp,
            repo,
            worktrees: root.join("worktrees"),
            locks: root.join("locks"),
            remote: None,
        }
    }

    pub fn with_remote() -> Fixture {
        let mut fx = Fixture::new();
        let bare = fx.repo.parent().unwrap().join("remote.git");
        fs::create_dir(&bare).unwrap();
        git(&bare, &["init", "-q", "--bare", "-b", "main"]);
        git(&bare, &["config", "core.hooksPath", "/dev/null"]);
        git(
            &fx.repo,
            &["remote", "add", "origin", bare.to_str().unwrap()],
        );
        git(&fx.repo, &["push", "-q", "origin", "main"]);
        fx.remote = Some(bare);
        fx
    }

    pub fn head(&self, dir: &Path, rev: &str) -> String {
        git(dir, &["rev-parse", rev])
    }
}

pub fn commit_file(dir: &Path, name: &str, content: &str, message: &str) -> String {
    fs::write(dir.join(name), content).unwrap();
    git(dir, &["add", "--", name]);
    git(dir, &["commit", "-q", "-m", message]);
    git(dir, &["rev-parse", "HEAD"])
}
