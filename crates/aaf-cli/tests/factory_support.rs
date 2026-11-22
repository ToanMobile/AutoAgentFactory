//! Shared fixture of the `factory_*` tests (included with `#[path]`; as a
//! test target of its own it holds no tests): a temp home, data dir and git
//! repository, and a way to run the real `aaf` binary against them.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::new_without_default
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use aaf_engine::db::Db;
use serde_json::Value;
use tempfile::TempDir;

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
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

pub struct Out {
    pub code: i32,
    pub json: Value,
    pub stderr: String,
}

pub struct Fixture {
    pub tmp: TempDir,
    /// Canonical path of the repository (as stored in `tasks.repo_root`).
    pub repo: PathBuf,
    pub data: PathBuf,
    pub worktrees: PathBuf,
}

impl Fixture {
    pub fn new() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        // Tier 1 of the child: its gate commands must not wait for the CPU
        // load of the machine that runs the tests.
        let tier1 = root.join("home/.config/autofactory");
        fs::create_dir_all(&tier1).unwrap();
        fs::write(
            tier1.join("config.toml"),
            "[machine]\nmax_load_percent = 0\n",
        )
        .unwrap();
        let repo = init_repo(&root, "repo");
        Fixture {
            repo,
            data: root.join("data"),
            worktrees: root.join("worktrees"),
            tmp,
        }
    }

    pub fn root(&self) -> PathBuf {
        fs::canonicalize(self.tmp.path()).unwrap()
    }

    /// `aaf --json <args>` in `cwd` with a scrubbed environment, not started.
    pub fn command(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_aaf"));
        command
            .arg("--json")
            .args(args)
            .current_dir(cwd)
            .env_clear()
            // The real PATH (git, sh), but nothing else of the developer's
            // environment: no HOME, no AAF_* variables, no agent run id.
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.root().join("home"))
            .env("AAF_DATA_DIR", &self.data)
            .env("AAF_WORKTREE_DIR", &self.worktrees);
        command
    }

    /// Runs `aaf --json <args>` in `cwd` with a scrubbed environment plus
    /// `extra_env`.
    pub fn aaf_in(&self, cwd: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> Out {
        let mut command = self.command(cwd, args);
        for (key, value) in extra_env {
            command.env(key, value);
        }
        let out = command.output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        let json = serde_json::from_str(stdout.trim()).unwrap_or(Value::Null);
        Out {
            code: out.status.code().unwrap_or(-1),
            json,
            stderr: format!("{stderr}\nstdout: {stdout}"),
        }
    }

    pub fn aaf(&self, args: &[&str]) -> Out {
        self.aaf_in(&self.repo, args, &[])
    }

    /// Adds a task and returns its id.
    pub fn add(&self, title: &str) -> String {
        let out = self.aaf(&[
            "board",
            "add",
            "--title",
            title,
            "--spec",
            "do the thing",
            "--accept",
            "it is done",
        ]);
        assert_eq!(out.code, 0, "{}", out.stderr);
        out.json["task"]["id"].as_str().unwrap().to_string()
    }

    /// The store, for arranging states the CLI has no command for. Dropped
    /// by the caller before `aaf` runs again.
    pub fn db(&self) -> Db {
        fs::create_dir_all(&self.data).unwrap();
        Db::open(&self.data.join("aaf.db")).unwrap()
    }

    pub fn show(&self, id: &str) -> Value {
        let out = self.aaf(&["board", "show", id]);
        assert_eq!(out.code, 0, "{}", out.stderr);
        out.json
    }

    pub fn state(&self, id: &str) -> String {
        self.show(id)["task"]["state"].as_str().unwrap().to_string()
    }
}

/// A repository on `main` with one commit and repo-local identity, so the
/// developer's global git config cannot change what the tests observe.
pub fn init_repo(root: &Path, name: &str) -> PathBuf {
    let repo = root.join(name);
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "--quiet", "-b", "main"]);
    for (key, value) in [
        ("user.name", "aaf test"),
        ("user.email", "aaf@example.invalid"),
        ("commit.gpgsign", "false"),
        ("core.hooksPath", "/dev/null"),
    ] {
        git(&repo, &["config", key, value]);
    }
    fs::write(repo.join("README.md"), "fixture\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "--quiet", "-m", "init"]);
    repo
}
