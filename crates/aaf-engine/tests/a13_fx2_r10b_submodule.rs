#![allow(clippy::unwrap_used, clippy::expect_used)]
//! O12-R10-4: a task worktree gets the submodules the main checkout has
//! checked out; a submodule that cannot be fetched leaves the worktree as
//! before (no `<git-dir>/modules` that would make a plain remove refuse).
//!
//! Production never allows the `file` transport for submodules. The test
//! that needs a local upstream runs `worktree::create` in a child copy of
//! this test binary whose global git config (the test's own file) allows it.

use std::fs;
use std::path::Path;
use std::process::Command;

use aaf_engine::git::worktree;

const CHILD: &str = "AAF_FX2_R10B_SUBMODULE_CHILD";

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "protocol.file.allow=always"])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
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
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `<tmp>/repo` whose main checkout has `<tmp>/up` checked out at `sub`.
fn superproject(tmp: &Path) -> std::path::PathBuf {
    let up = tmp.join("up");
    let repo = tmp.join("repo");
    for dir in [&up, &repo] {
        fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q", "-b", "main"]);
    }
    fs::write(up.join("lib.txt"), "from the submodule\n").unwrap();
    git(&up, &["add", "-A"]);
    git(&up, &["commit", "-q", "-m", "up"]);
    fs::write(repo.join("r.txt"), "r\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    git(
        &repo,
        &["submodule", "add", "-q", up.to_str().unwrap(), "sub"],
    );
    git(&repo, &["commit", "-q", "-m", "add submodule"]);
    repo
}

fn child_create(dir: &Path) {
    worktree::create(&dir.join("repo"), &dir.join("worktrees"), "t1", "main").unwrap();
}

#[test]
fn task_worktree_has_the_submodules_of_the_main_checkout() {
    if let Ok(dir) = std::env::var(CHILD) {
        child_create(Path::new(&dir));
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let tmp_path = fs::canonicalize(tmp.path()).unwrap();
    let repo = superproject(&tmp_path);
    let config = tmp_path.join("gitconfig");
    fs::write(&config, "[protocol \"file\"]\n\tallow = always\n").unwrap();
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "task_worktree_has_the_submodules_of_the_main_checkout",
            "--test-threads=1",
        ])
        .env(CHILD, &tmp_path)
        .env("GIT_CONFIG_GLOBAL", &config)
        .status()
        .unwrap();
    assert!(status.success(), "child create failed");

    let wt = worktree::worktree_path(&repo, &tmp_path.join("worktrees"), "t1").unwrap();
    assert_eq!(
        fs::read_to_string(wt.join("sub/lib.txt")).unwrap_or_default(),
        "from the submodule\n",
        "the task worktree did not get the submodule's files"
    );
    assert!(git(&wt, &["status", "--porcelain"]).trim().is_empty());
    // A clean task worktree with a checked-out submodule still goes away
    // through the normal, unforced cleanup.
    let tree = worktree::TaskWorktree {
        path: wt.clone(),
        branch: "aaf/t1".to_string(),
    };
    git(&repo, &["branch", "-f", "keep", "aaf/t1"]);
    worktree::remove(&repo, &tree, true, false).unwrap();
    assert!(!wt.exists());
}

#[test]
fn an_unfetchable_submodule_leaves_a_plain_removable_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp_path = fs::canonicalize(tmp.path()).unwrap();
    let repo = superproject(&tmp_path);
    // In this process the `file` transport is not allowed for submodules,
    // as in production: the clone fails.
    let tree = worktree::create(&repo, &tmp_path.join("worktrees"), "t2", "main").unwrap();
    assert!(tree.path.join("r.txt").is_file());
    assert!(!tree.path.join("sub/lib.txt").exists());
    assert_eq!(worktree::removal_refusal(&tree.path).unwrap(), None);
    assert!(
        git(&tree.path, &["status", "--porcelain"])
            .trim()
            .is_empty()
    );
}

#[test]
fn an_uninitialised_submodule_stays_uninitialised() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp_path = fs::canonicalize(tmp.path()).unwrap();
    let repo = superproject(&tmp_path);
    git(&repo, &["submodule", "deinit", "-q", "--force", "sub"]);
    let tree = worktree::create(&repo, &tmp_path.join("worktrees"), "t3", "main").unwrap();
    assert!(!tree.path.join("sub/lib.txt").exists());
    assert_eq!(worktree::removal_refusal(&tree.path).unwrap(), None);
}
