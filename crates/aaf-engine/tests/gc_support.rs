//! Shared fixture for the `aaf gc` integration tests (included with
//! `#[path]`; as a test target of its own it holds no tests).
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use aaf_domain::ports::TaskStore;
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;

/// Runs git for test setup and returns its stdout; panics on failure.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "protocol.file.allow=always"])
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
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A repository on `main` with one commit. Repo-local settings keep the
/// developer's global git config (identity, signing, hooks) out of the test.
pub fn init_repo(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    for (key, value) in [
        ("user.name", "Tester"),
        ("user.email", "test@example.com"),
        ("commit.gpgsign", "false"),
        ("core.hooksPath", "/dev/null"),
    ] {
        git(dir, &["config", key, value]);
    }
    fs::write(dir.join("README.md"), "hello\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

pub fn done_task(db: &Db, id: &str, repo: &Path) {
    let spec = ImmutableSpec {
        description: "d".to_string(),
        acceptance: vec!["ok".to_string()],
    };
    let mut task = Task::new_root(id, repo.to_string_lossy(), id, spec, Priority::P2, 1_000);
    task.state = TaskState::Done;
    db.insert_task(&task).unwrap();
}

pub fn has_branch(repo: &Path, branch: &str) -> bool {
    !git(repo, &["branch", "--list", branch]).trim().is_empty()
}

/// A stream log 20 days old under `data`, which gc's artifact sweep removes.
pub fn expired_stream(data: &Path) -> PathBuf {
    let streams = data.join("streams");
    fs::create_dir_all(&streams).unwrap();
    let old = streams.join("x.jsonl");
    fs::write(&old, "{}\n").unwrap();
    let age = Duration::from_secs(20 * 86_400);
    File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(SystemTime::now().checked_sub(age).unwrap())
        .unwrap();
    old
}
