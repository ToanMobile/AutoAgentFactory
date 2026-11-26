#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Verification oracle for GATE-089: head-lock must inspect nested git
//! repositories and gitlinks even when their directory path is not itself a
//! locked directory prefix, blocking locked files inside them while leaving
//! clean nested repos and non-locked submodule edits Clear.

use std::fs;
use std::path::Path;
use std::process::Command;

use aaf_domain::config::RepoConfig;
use aaf_domain::gate::{ChangeKind, LockStatus, LockTier};
use aaf_engine::gate::{GIT_TIMEOUT, changes, head_lock};
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "protocol.file.allow=always",
        ])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.name", "test"]);
    git(dir, &["config", "user.email", "test@example.invalid"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
}

fn make_base_repo() -> (TempDir, TempDir, String) {
    let repo = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    let root = repo.path();
    init_repo(root);
    fs::write(root.join("notes.txt"), "base notes\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "base"]);
    let base_sha = git(root, &["rev-parse", "HEAD"]);
    git(root, &["checkout", "-q", "-b", "aaf/task"]);
    (repo, data, base_sha)
}

fn make_base_repo_with_submodule() -> (TempDir, TempDir, TempDir, String) {
    let upstream = TempDir::new().unwrap();
    init_repo(upstream.path());
    fs::create_dir_all(upstream.path().join("src")).unwrap();
    fs::write(
        upstream.path().join("Cargo.toml"),
        "[package]\nname = \"sub\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        upstream.path().join("src/lib.rs"),
        "pub fn value() -> i32 { 1 }\n",
    )
    .unwrap();
    git(upstream.path(), &["add", "-A"]);
    git(upstream.path(), &["commit", "-q", "-m", "sub base"]);

    let repo = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    let root = repo.path();
    init_repo(root);
    fs::write(root.join("notes.txt"), "base notes\n").unwrap();
    let upstream_str = upstream.path().to_str().unwrap();
    git(
        root,
        &["submodule", "add", "-q", upstream_str, "vendor/sub"],
    );
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "base with submodule"]);
    let base_sha = git(root, &["rev-parse", "HEAD"]);
    git(root, &["checkout", "-q", "-b", "aaf/task"]);
    (repo, data, upstream, base_sha)
}

fn create_nested_repo(dir: &Path, files: &[(&str, &str)]) {
    fs::create_dir_all(dir).unwrap();
    init_repo(dir);
    for (rel, content) in files {
        let full = dir.join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(full, content).unwrap();
    }
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "nested init"]);
}

fn path_under(path: &str, prefix: &str) -> bool {
    let trimmed = path.trim_end_matches('/');
    trimmed == prefix
        || trimmed
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

#[test]
fn added_gitlink_with_cargo_toml_and_build_rs_is_hard_blocked() {
    let (repo, data, base_sha) = make_base_repo();
    let root = repo.path();
    create_nested_repo(
        &root.join("crates/evil"),
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"evil\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("build.rs", "fn main() {}\n"),
        ],
    );
    git(root, &["add", "crates/evil"]);
    git(root, &["commit", "-q", "-m", "add crates/evil gitlink"]);

    let tree_entry = git(root, &["ls-tree", "HEAD", "crates/evil"]);
    assert!(
        tree_entry.starts_with("160000 commit "),
        "expected gitlink mode 160000 for crates/evil, got: {tree_entry}"
    );

    let set = changes::collect(root, &base_sha, GIT_TIMEOUT).unwrap();
    assert!(
        set.changes
            .iter()
            .any(|c| c.kind == ChangeKind::Added && path_under(&c.path, "crates/evil")),
        "expected Added change for crates/evil, got: {:?}",
        set.changes
    );

    let report = head_lock::evaluate(data.path(), root, &RepoConfig::default(), &base_sha);
    assert_eq!(
        report.status,
        LockStatus::Blocked,
        "added gitlink crates/evil containing Cargo.toml + build.rs must be Blocked, got: {report:?}"
    );
    assert!(
        report
            .locked_changes
            .iter()
            .any(|c| path_under(&c.path, "crates/evil") && c.tier == LockTier::Hard),
        "expected Hard locked entry under crates/evil, got: {report:?}"
    );
}

#[test]
fn added_untracked_nested_repo_with_conftest_is_hard_blocked() {
    let (repo, data, base_sha) = make_base_repo();
    let root = repo.path();
    create_nested_repo(
        &root.join("tests/sub"),
        &[("conftest.py", "def pytest_configure(config): pass\n")],
    );

    let set = changes::collect(root, &base_sha, GIT_TIMEOUT).unwrap();
    assert!(
        set.changes
            .iter()
            .any(|c| c.kind == ChangeKind::Added && path_under(&c.path, "tests/sub")),
        "expected Added change for untracked tests/sub/, got: {:?}",
        set.changes
    );

    let report = head_lock::evaluate(data.path(), root, &RepoConfig::default(), &base_sha);
    assert_eq!(
        report.status,
        LockStatus::Blocked,
        "untracked nested repo tests/sub/ containing conftest.py must be Blocked, got: {report:?}"
    );
    assert!(
        report
            .locked_changes
            .iter()
            .any(|c| path_under(&c.path, "tests/sub") && c.tier == LockTier::Hard),
        "expected Hard locked entry under tests/sub, got: {report:?}"
    );
}

#[test]
fn modified_submodule_editing_cargo_toml_is_hard_blocked() {
    let (repo, data, _upstream, base_sha) = make_base_repo_with_submodule();
    let root = repo.path();
    let sub = root.join("vendor/sub");
    fs::write(
        sub.join("Cargo.toml"),
        "[package]\nname = \"sub\"\nversion = \"0.2.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    git(&sub, &["add", "Cargo.toml"]);
    git(
        &sub,
        &["commit", "-q", "-m", "edit Cargo.toml in submodule"],
    );
    git(root, &["add", "vendor/sub"]);
    git(root, &["commit", "-q", "-m", "bump submodule Cargo.toml"]);

    let set = changes::collect(root, &base_sha, GIT_TIMEOUT).unwrap();
    assert!(
        set.changes
            .iter()
            .any(|c| c.kind == ChangeKind::Modified && path_under(&c.path, "vendor/sub")),
        "expected Modified change for vendor/sub, got: {:?}",
        set.changes
    );

    let report = head_lock::evaluate(data.path(), root, &RepoConfig::default(), &base_sha);
    assert_eq!(
        report.status,
        LockStatus::Blocked,
        "submodule commit modifying Cargo.toml must be Blocked, got: {report:?}"
    );
    assert!(
        report
            .locked_changes
            .iter()
            .any(|c| path_under(&c.path, "vendor/sub") && c.tier == LockTier::Hard),
        "expected Hard locked entry under vendor/sub, got: {report:?}"
    );
}

#[test]
fn modified_submodule_editing_only_src_lib_stays_clear() {
    let (repo, data, _upstream, base_sha) = make_base_repo_with_submodule();
    let root = repo.path();
    let sub = root.join("vendor/sub");
    fs::write(sub.join("src/lib.rs"), "pub fn value() -> i32 { 2 }\n").unwrap();
    git(&sub, &["add", "src/lib.rs"]);
    git(&sub, &["commit", "-q", "-m", "edit src/lib.rs only"]);
    git(root, &["add", "vendor/sub"]);
    git(root, &["commit", "-q", "-m", "bump submodule src/lib.rs"]);

    let set = changes::collect(root, &base_sha, GIT_TIMEOUT).unwrap();
    assert!(
        set.changes
            .iter()
            .any(|c| c.kind == ChangeKind::Modified && path_under(&c.path, "vendor/sub")),
        "expected Modified change for vendor/sub, got: {:?}",
        set.changes
    );

    let report = head_lock::evaluate(data.path(), root, &RepoConfig::default(), &base_sha);
    assert_eq!(
        report.status,
        LockStatus::Clear,
        "submodule commit editing only src/lib.rs must stay Clear, got: {report:?}"
    );
    assert!(
        report.locked_changes.is_empty(),
        "expected no locked entries for non-locked submodule edit, got: {report:?}"
    );
}

#[test]
fn modified_uninitialized_gitlink_fails_closed_blocked() {
    let (repo, data, _upstream, base_sha) = make_base_repo_with_submodule();
    let root = repo.path();
    let sub = root.join("vendor/sub");
    fs::write(
        sub.join("Cargo.toml"),
        "[package]\nname = \"sub\"\nversion = \"0.3.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    git(&sub, &["add", "Cargo.toml"]);
    git(&sub, &["commit", "-q", "-m", "edit Cargo.toml"]);
    git(root, &["add", "vendor/sub"]);
    git(root, &["commit", "-q", "-m", "bump submodule sha"]);

    // Deinitialize submodule so vendor/sub is an empty uninitialized directory.
    git(root, &["submodule", "deinit", "-f", "vendor/sub"]);
    assert!(sub.is_dir(), "expected empty mount directory at vendor/sub");
    assert!(
        fs::read_dir(&sub).unwrap().next().is_none(),
        "expected vendor/sub to be empty after deinit"
    );

    let set = changes::collect(root, &base_sha, GIT_TIMEOUT).unwrap();
    assert!(
        set.changes
            .iter()
            .any(|c| c.kind == ChangeKind::Modified && path_under(&c.path, "vendor/sub")),
        "expected Modified change for uninitialized vendor/sub gitlink, got: {:?}",
        set.changes
    );

    let report = head_lock::evaluate(data.path(), root, &RepoConfig::default(), &base_sha);
    assert_eq!(
        report.status,
        LockStatus::Blocked,
        "modified uninitialized gitlink must fail closed as Blocked, got: {report:?}"
    );
    if !report.locked_changes.is_empty() {
        assert!(
            report.has_hard_lock(),
            "locked entry for uninitialized gitlink must be Hard, got: {report:?}"
        );
    }
}

#[test]
fn added_clean_nested_repo_without_locked_files_stays_clear() {
    let (repo, data, base_sha) = make_base_repo();
    let root = repo.path();
    create_nested_repo(
        &root.join("docs/notes"),
        &[("readme.txt", "plain notes without locked globs\n")],
    );

    let untracked_report =
        head_lock::evaluate(data.path(), root, &RepoConfig::default(), &base_sha);
    assert_eq!(
        untracked_report.status,
        LockStatus::Clear,
        "untracked nested repo docs/notes/ with only readme.txt must stay Clear, got: {untracked_report:?}"
    );
    assert!(
        untracked_report.locked_changes.is_empty(),
        "expected no locked entries for clean untracked nested repo, got: {untracked_report:?}"
    );

    git(root, &["add", "docs/notes"]);
    git(root, &["commit", "-q", "-m", "add clean nested repo"]);

    let committed_report =
        head_lock::evaluate(data.path(), root, &RepoConfig::default(), &base_sha);
    assert_eq!(
        committed_report.status,
        LockStatus::Clear,
        "committed gitlink docs/notes with only readme.txt must stay Clear, got: {committed_report:?}"
    );
    assert!(
        committed_report.locked_changes.is_empty(),
        "expected no locked entries for clean committed gitlink, got: {committed_report:?}"
    );
}
