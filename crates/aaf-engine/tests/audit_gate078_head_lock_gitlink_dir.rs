#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Verification test for GATE-078: head-lock must treat a gitlink or untracked
//! nested git repository placed at a locked config directory (e.g. `.cargo`) as
//! locked, just like a symlink at the same directory prefix.

use std::fs;
use std::path::Path;
use std::process::Command;

use aaf_domain::config::RepoConfig;
use aaf_domain::gate::{ChangeKind, LockStatus};
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

fn create_nested_cargo_repo(root: &Path) {
    let cargo_dir = root.join(".cargo");
    fs::create_dir_all(&cargo_dir).unwrap();
    init_repo(&cargo_dir);
    fs::write(
        cargo_dir.join("config.toml"),
        "[build]\nrustc = \"/bin/true\"\n",
    )
    .unwrap();
    git(&cargo_dir, &["add", "-A"]);
    git(&cargo_dir, &["commit", "-q", "-m", "nested cargo config"]);
}

fn is_cargo_path(path: &str) -> bool {
    path == ".cargo" || path.starts_with(".cargo/")
}

#[test]
fn committed_gitlink_at_locked_dir_is_head_locked() {
    let (repo, data, base_sha) = make_base_repo();
    let root = repo.path();
    create_nested_cargo_repo(root);
    fs::write(
        root.join(".gitmodules"),
        "[submodule \".cargo\"]\n\tpath = .cargo\n\turl = ./.cargo\n",
    )
    .unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "add .cargo gitlink"]);

    assert!(root.join(".cargo/config.toml").is_file());
    let tree_entry = git(root, &["ls-tree", "HEAD", ".cargo"]);
    assert!(
        tree_entry.starts_with("160000 commit "),
        "expected gitlink mode 160000 for .cargo, got: {tree_entry}"
    );

    let set = changes::collect(root, &base_sha, GIT_TIMEOUT).unwrap();
    assert!(
        set.changes
            .iter()
            .any(|c| c.kind == ChangeKind::Added && is_cargo_path(&c.path)),
        "expected Added change for .cargo, got: {:?}",
        set.changes
    );

    let report = head_lock::evaluate(data.path(), root, &RepoConfig::default(), &base_sha);
    assert_eq!(
        report.status,
        LockStatus::Blocked,
        "committed gitlink at .cargo must be head-locked, got: {report:?}"
    );
    assert!(
        report.locked_changes.iter().any(|c| is_cargo_path(&c.path)),
        "expected locked entry for .cargo, got: {report:?}"
    );
}

#[test]
fn untracked_nested_repo_at_locked_dir_is_head_locked() {
    let (repo, data, base_sha) = make_base_repo();
    let root = repo.path();
    create_nested_cargo_repo(root);

    assert!(root.join(".cargo/config.toml").is_file());
    let set = changes::collect(root, &base_sha, GIT_TIMEOUT).unwrap();
    assert!(
        set.changes
            .iter()
            .any(|c| c.kind == ChangeKind::Added && is_cargo_path(&c.path)),
        "expected Added change for untracked .cargo/, got: {:?}",
        set.changes
    );

    let report = head_lock::evaluate(data.path(), root, &RepoConfig::default(), &base_sha);
    assert_eq!(
        report.status,
        LockStatus::Blocked,
        "untracked nested git repo at .cargo/ must be head-locked, got: {report:?}"
    );
    assert!(
        report.locked_changes.iter().any(|c| is_cargo_path(&c.path)),
        "expected locked entry for .cargo, got: {report:?}"
    );
}

#[cfg(unix)]
#[test]
fn symlink_at_locked_dir_is_head_locked() {
    let (repo, data, base_sha) = make_base_repo();
    let root = repo.path();
    let helper = root.join("helper");
    fs::create_dir_all(&helper).unwrap();
    fs::write(
        helper.join("config.toml"),
        "[build]\nrustc = \"/bin/true\"\n",
    )
    .unwrap();
    std::os::unix::fs::symlink("helper", root.join(".cargo")).unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "add .cargo symlink"]);

    let report = head_lock::evaluate(data.path(), root, &RepoConfig::default(), &base_sha);
    assert_eq!(
        report.status,
        LockStatus::Blocked,
        "symlink .cargo -> helper must be head-locked, got: {report:?}"
    );
    assert!(
        report.locked_changes.iter().any(|c| c.path == ".cargo"),
        "expected locked entry for .cargo symlink, got: {report:?}"
    );
}

#[test]
fn plain_new_dir_outside_locked_globs_stays_clear() {
    let (repo, data, base_sha) = make_base_repo();
    let root = repo.path();
    fs::create_dir_all(root.join("docs/notes")).unwrap();
    fs::write(root.join("docs/notes/a.md"), "# Notes\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "add plain doc"]);

    let report = head_lock::evaluate(data.path(), root, &RepoConfig::default(), &base_sha);
    assert_eq!(
        report.status,
        LockStatus::Clear,
        "plain new dir outside locked globs must stay Clear, got: {report:?}"
    );
    assert!(
        report.locked_changes.is_empty(),
        "expected no locked entries, got: {report:?}"
    );
}
