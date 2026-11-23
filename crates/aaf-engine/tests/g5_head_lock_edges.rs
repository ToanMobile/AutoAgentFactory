#![allow(clippy::unwrap_used, clippy::expect_used)]
//! G5 extra edge-case tests for head-lock runner configs and rename bypass.

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

fn base_repo() -> (TempDir, TempDir, String) {
    let repo = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    let root = repo.path();
    git(root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("notes.txt"), "notes\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "base"]);
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    (
        repo,
        data,
        String::from_utf8(out.stdout).unwrap().trim().to_string(),
    )
}

#[test]
fn g5_added_runner_build_lint_and_nested_configs_are_blocked() {
    let cfg = RepoConfig::default();
    for rel in [
        "android/gradlew",
        "android/gradle/wrapper/gradle-wrapper.properties",
        "buildSrc/src/main/kotlin/Plugin.kt",
        "tests/conftest.py",
        "tests/Conftest.py",
        "tests/jest.config.js",
        "tests/.claude/settings.json",
        "pyproject.toml",
        "Makefile",
        "makefile",
        "go.mod",
        "go.work",
        "cargo.toml",
        ".Cargo/config.toml",
        "claude.md",
        "pubspec.yaml",
        "playwright.config.ts",
        "eslint.config.mjs",
        "Package.swift",
        "fastlane/Fastfile",
        "Assets/Game.asmdef",
    ] {
        let (repo, data, base_sha) = base_repo();
        let full = repo.path().join(rel);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(&full, "x\n").unwrap();
        git(repo.path(), &["add", "-A"]);
        let report = head_lock::evaluate(data.path(), repo.path(), &cfg, &base_sha);
        assert_eq!(
            report.status,
            LockStatus::Blocked,
            "expected Blocked: {rel}"
        );
    }
}

#[test]
fn g5_rename_into_cargo_config_is_blocked() {
    let (repo, data, base_sha) = base_repo();
    fs::create_dir_all(repo.path().join(".cargo")).unwrap();
    git(repo.path(), &["mv", "notes.txt", ".cargo/config.toml"]);
    let report = head_lock::evaluate(data.path(), repo.path(), &RepoConfig::default(), &base_sha);
    assert_eq!(report.status, LockStatus::Blocked);
}

#[test]
fn g5_rename_unlocked_file_into_pure_test_file_stays_clear() {
    let (repo, data, base_sha) = base_repo();
    fs::create_dir_all(repo.path().join("tests")).unwrap();
    git(repo.path(), &["mv", "notes.txt", "tests/new_test.rs"]);
    let report = head_lock::evaluate(data.path(), repo.path(), &RepoConfig::default(), &base_sha);
    assert_eq!(report.status, LockStatus::Clear);
}

#[test]
fn g5_rename_unlocked_file_into_tests_conftest_is_blocked() {
    let (repo, data, base_sha) = base_repo();
    fs::create_dir_all(repo.path().join("tests")).unwrap();
    git(repo.path(), &["mv", "notes.txt", "tests/conftest.py"]);
    let report = head_lock::evaluate(data.path(), repo.path(), &RepoConfig::default(), &base_sha);
    assert_eq!(report.status, LockStatus::Blocked);
}

#[test]
fn g5_case_sensitive_test_globs_do_not_false_lock_non_test_source() {
    let (repo, data, _) = base_repo();
    fs::create_dir_all(repo.path().join("src")).unwrap();
    fs::write(repo.path().join("src/latest.kt"), "fun x() = 1\n").unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-q", "-m", "add latest.kt"]);
    let out = Command::new("git")
        .arg("-C")
        .arg(repo.path())
        .args(["rev-parse", "HEAD"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    let base_sha = String::from_utf8(out.stdout).unwrap().trim().to_string();

    fs::write(repo.path().join("src/latest.kt"), "fun x() = 2\n").unwrap();
    let report = head_lock::evaluate(data.path(), repo.path(), &RepoConfig::default(), &base_sha);
    assert_eq!(report.status, LockStatus::Clear);
}

#[cfg(unix)]
#[test]
fn g5_symlink_shadowing_locked_config_dir_is_blocked() {
    use std::os::unix::fs::symlink;
    let cfg = RepoConfig::default();

    for link_rel in [".cargo", ".Cargo", ".claude", ".github", "sub/.cargo"] {
        let (repo, data, base_sha) = base_repo();
        let helper = repo.path().join("helper");
        fs::create_dir_all(&helper).unwrap();
        fs::write(
            helper.join("config.toml"),
            "[target.x]\nrunner = \"true\"\n",
        )
        .unwrap();
        let link = repo.path().join(link_rel);
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(&helper, &link).unwrap();

        // Blocked even when untracked, and also when staged.
        let report = head_lock::evaluate(data.path(), repo.path(), &cfg, &base_sha);
        assert_eq!(report.status, LockStatus::Blocked, "untracked {link_rel}");
        git(repo.path(), &["add", "-A"]);
        let report = head_lock::evaluate(data.path(), repo.path(), &cfg, &base_sha);
        assert_eq!(report.status, LockStatus::Blocked, "staged {link_rel}");
    }

    // Benign symlink outside any locked directory stays Clear.
    let (repo, data, base_sha) = base_repo();
    fs::create_dir_all(repo.path().join("docs")).unwrap();
    symlink(repo.path().join("notes.txt"), repo.path().join("docs/link")).unwrap();
    let report = head_lock::evaluate(data.path(), repo.path(), &cfg, &base_sha);
    assert_eq!(report.status, LockStatus::Clear);
}

#[test]
fn g5_custom_tier2_glob_locks_newly_added_file() {
    let (repo, data, base_sha) = base_repo();
    let mut cfg = RepoConfig::default();
    cfg.gate
        .head_lock_globs
        .push("**/.custom-runner.yml".to_string());
    fs::write(repo.path().join(".custom-runner.yml"), "steps: []\n").unwrap();
    git(repo.path(), &["add", "-A"]);
    let report = head_lock::evaluate(data.path(), repo.path(), &cfg, &base_sha);
    assert_eq!(report.status, LockStatus::Blocked);
}
