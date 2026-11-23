#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Oracle G5: head-lock for test-runner configs + rename bypass fix.
//!
//! Phase 0 — written against base 92367ec BEFORE any production edits.
//! **NEVER edit this file after Phase 0.**
//!
//! Expected on base: Test 1 and Test 3 fail at assertion (LockStatus::Clear
//! when Blocked is expected); Test 2 passes.

use std::fs;
use std::path::Path;
use std::process::Command;

use aaf_domain::config::RepoConfig;
use aaf_domain::gate::LockStatus;
use aaf_engine::gate::head_lock;
use tempfile::TempDir;

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn make_base_repo() -> (TempDir, TempDir, String) {
    let repo = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    let root = repo.path();
    git(root, &["init", "-q", "-b", "main"]);
    // Seed repo with a benign file so we have a base commit.
    fs::write(root.join("notes.txt"), "some notes\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "base"]);
    // Capture base SHA.
    let base_sha = String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "HEAD"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    (repo, data, base_sha)
}

/// Test 1: Adding a new `.cargo/config.toml` must be Blocked (a runner config
/// is as dangerous as an agent config). On base 92367ec this returns Clear
/// because only AGENT_CONFIG_GLOBS triggers the Added-exemption bypass.
///
/// **Oracle assertion — expected RED on base.**
#[test]
fn oracle_g5_test1_new_cargo_config_is_blocked() {
    let (repo, data, base_sha) = make_base_repo();
    let root = repo.path();
    let cfg = RepoConfig::default();

    // Add .cargo/config.toml (a runner config — not yet locked for Added on base).
    fs::create_dir_all(root.join(".cargo")).unwrap();
    fs::write(
        root.join(".cargo/config.toml"),
        "[build]\ntarget = \"aarch64\"\n",
    )
    .unwrap();
    git(root, &["add", "-A"]);

    let report = head_lock::evaluate(data.path(), root, &cfg, &base_sha);
    // On base 92367ec: returns LockStatus::Clear  <-- RED here
    assert_eq!(
        report.status,
        LockStatus::Blocked,
        "oracle G5 test1: adding .cargo/config.toml must be Blocked (got {:?})",
        report.status
    );
}

/// Test 2: Adding a brand-new test file must remain Clear (test-file exemption).
///
/// **Oracle assertion — expected GREEN on base (this test already passes).**
#[test]
fn oracle_g5_test2_new_test_file_is_clear() {
    let (repo, data, base_sha) = make_base_repo();
    let root = repo.path();
    let cfg = RepoConfig::default();

    // Add a new test file.
    fs::create_dir_all(root.join("tests")).unwrap();
    fs::write(
        root.join("tests/new_feature_test.rs"),
        "#[test]\nfn it_works() {}\n",
    )
    .unwrap();
    git(root, &["add", "-A"]);

    let report = head_lock::evaluate(data.path(), root, &cfg, &base_sha);
    assert_eq!(
        report.status,
        LockStatus::Clear,
        "oracle G5 test2: adding a test file must stay Clear (got {:?})",
        report.status
    );
}

/// Test 3: `git mv notes.txt .claude/settings.json` (rename benign -> locked)
/// must be Blocked. On base 92367ec only `old_path` is checked, so renaming a
/// file that was not locked under its original name returns Clear.
///
/// **Oracle assertion — expected RED on base.**
#[test]
fn oracle_g5_test3_rename_into_locked_path_is_blocked() {
    let (repo, data, base_sha) = make_base_repo();
    let root = repo.path();
    let cfg = RepoConfig::default();

    // git mv notes.txt .claude/settings.json
    fs::create_dir_all(root.join(".claude")).unwrap();
    git(root, &["mv", "notes.txt", ".claude/settings.json"]);

    let report = head_lock::evaluate(data.path(), root, &cfg, &base_sha);
    // On base 92367ec: only old_path ("notes.txt") is checked -> Clear  <-- RED here
    assert_eq!(
        report.status,
        LockStatus::Blocked,
        "oracle G5 test3: renaming into .claude/settings.json must be Blocked (got {:?})",
        report.status
    );
}
