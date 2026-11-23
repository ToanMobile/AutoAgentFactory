#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `gate.base_ref = "auto"` follows `refs/remotes/origin/HEAD` only as a
//! symbolic ref to another (non-task) branch of `origin` that shares
//! history with `HEAD`, and never falls back to `HEAD` past a broken one
//! (GATE-012).

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use aaf_domain::config::RepoConfig;
use aaf_domain::gate::LockStatus;
use aaf_engine::factory::resolve_base;
use aaf_engine::gate::{GateError, base, head_lock};
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(30);

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
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
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn commit_file(root: &Path, rel: &str, body: &str) -> String {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", rel]);
    git(root, &["rev-parse", "HEAD"])
}

/// A repo whose `trunk` (holding a head-locked test) is pushed to a bare
/// `origin`, next to an orphan `origin/gh-pages` (no shared history).
/// Returns `(origin, repo, trunk sha)`.
fn pushed(trunk: &str) -> (TempDir, TempDir, String) {
    let origin = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    git(origin.path(), &["init", "-q", "--bare", "-b", trunk]);
    git(root, &["init", "-q", "-b", trunk]);
    let url = origin.path().to_str().unwrap();
    git(root, &["remote", "add", "origin", url]);
    let sha = commit_file(root, "tests/a_test.rs", "fn t() { assert!(true); }\n");
    git(root, &["push", "-q", "origin", trunk]);
    let tree = git(root, &["hash-object", "-w", "-t", "tree", "/dev/null"]);
    let pages = git(root, &["commit-tree", &tree, "-m", "pages"]);
    git(
        root,
        &["update-ref", "refs/remotes/origin/gh-pages", &pages],
    );
    (origin, repo, sha)
}

fn auto_base(root: &Path) -> Option<String> {
    base::resolve(root, "auto", TIMEOUT).unwrap().sha
}

#[test]
fn a_remote_symref_origin_head_is_followed_and_a_clean_change_stays_clear() {
    let (_origin, repo, main) = pushed("main");
    let root = repo.path();
    // The default branch is `develop`, ahead of `main`.
    git(root, &["checkout", "-q", "-b", "develop"]);
    let develop = commit_file(root, "src/d.rs", "pub fn d() {}\n");
    git(root, &["push", "-q", "origin", "develop"]);
    let target = "refs/remotes/origin/develop";
    git(root, &["symbolic-ref", "refs/remotes/origin/HEAD", target]);
    git(root, &["checkout", "-q", "-b", "feature"]);
    commit_file(root, "src/f.rs", "pub fn f() {}\n");

    let sha = auto_base(root).unwrap();
    assert_eq!(sha, develop, "origin/HEAD -> origin/develop must be used");
    let data = TempDir::new().unwrap();
    let report = head_lock::evaluate(data.path(), root, &RepoConfig::default(), &sha);
    assert_eq!(report.status, LockStatus::Clear);

    let target = "refs/remotes/origin/main";
    git(root, &["symbolic-ref", "refs/remotes/origin/HEAD", target]);
    assert_eq!(auto_base(root), Some(main));
}

#[test]
fn a_task_or_orphan_origin_head_falls_back_to_origin_main() {
    let (_origin, repo, main) = pushed("main");
    let root = repo.path();
    git(root, &["checkout", "-q", "-b", "aaf/task-1"]);
    commit_file(root, "tests/a_test.rs", "fn t() {}\n");
    // The task branch, as `open_pr` leaves it after a push.
    let pushed_task = "refs/remotes/origin/aaf/task-1";
    git(root, &["update-ref", pushed_task, "HEAD"]);
    let origin_main = resolve_base(root, "origin/main").unwrap();
    for target in [
        "refs/heads/aaf/task-1",
        "refs/remotes/origin/aaf/task-1",
        "refs/remotes/origin/gh-pages",
    ] {
        git(root, &["symbolic-ref", "refs/remotes/origin/HEAD", target]);
        let sha = auto_base(root).unwrap();
        assert_eq!(sha, main, "{target}: must fall back to origin/main");
        let data = TempDir::new().unwrap();
        let report = head_lock::evaluate(data.path(), root, &RepoConfig::default(), &sha);
        assert_eq!(report.status, LockStatus::Blocked, "{target}");
        if target.contains("/aaf/") {
            assert_eq!(resolve_base(root, "auto").unwrap(), origin_main, "{target}");
        }
    }
}

#[test]
fn a_broken_origin_head_without_a_usable_trunk_is_an_error_not_head() {
    let (_origin, repo, develop) = pushed("develop");
    let root = repo.path();
    let target = "refs/remotes/origin/develop";
    git(root, &["symbolic-ref", "refs/remotes/origin/HEAD", target]);
    git(root, &["checkout", "-q", "-b", "feature"]);
    commit_file(root, "tests/a_test.rs", "fn t() {}\n");
    assert_eq!(auto_base(root), Some(develop));

    let blob = git(root, &["rev-parse", "HEAD:tests/a_test.rs"]);
    let head = git(root, &["rev-parse", "HEAD"]);
    let (sym, direct) = (["symbolic-ref", "-q"], ["update-ref", "--no-deref"]);
    for ([cmd, flag], value) in [
        (sym, "refs/remotes/origin/missing"),
        (sym, "refs/remotes/origin/gh-pages"),
        (direct, blob.as_str()),
        (direct, head.as_str()),
    ] {
        git(root, &[cmd, flag, "refs/remotes/origin/HEAD", value]);
        let result = base::resolve(root, "auto", TIMEOUT);
        assert!(matches!(result, Err(GateError::Config(_))), "{result:?}");
    }
}
