#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Audit round 3 (moved from `audit_round3_gate.rs` to keep it under 600
//! lines): the git layout probe and the layout-based tree fingerprint agree
//! with the plain ones in a primary checkout and in a linked worktree.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(30);

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
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
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn probe_git_layout_and_tree_fp_with_layout_match_primary_and_worktree() {
    let dir = TempDir::new().unwrap();
    let primary = dir.path();
    git(primary, &["init", "-q", "-b", "main"]);
    fs::write(primary.join("a.txt"), "hello\n").unwrap();
    git(primary, &["add", "-A"]);
    git(primary, &["commit", "-q", "-m", "init"]);

    let canon_primary = fs::canonicalize(primary).unwrap();
    let layout = aaf_fp::probe_git_layout(&canon_primary, TIMEOUT).unwrap();
    assert_eq!(layout.git_dir, canon_primary.join(".git"));
    assert_eq!(layout.common_dir, canon_primary.join(".git"));
    assert_eq!(layout.index, canon_primary.join(".git/index"));
    assert_eq!(layout.objects, canon_primary.join(".git/objects"));
    assert_eq!(layout.common_repo_root(), canon_primary.as_path());
    assert_eq!(
        aaf_fp::tree_fp_with_layout(&canon_primary, &layout, TIMEOUT).unwrap(),
        aaf_fp::tree_fp(&canon_primary, TIMEOUT).unwrap()
    );

    let wt_parent = TempDir::new().unwrap();
    let wt = wt_parent.path().join("linked-wt");
    git(
        primary,
        &["worktree", "add", "--detach", wt.to_str().unwrap(), "HEAD"],
    );
    let canon_wt = fs::canonicalize(&wt).unwrap();
    let wt_layout = aaf_fp::probe_git_layout(&canon_wt, TIMEOUT).unwrap();
    assert_ne!(wt_layout.git_dir, wt_layout.common_dir);
    assert_eq!(wt_layout.common_dir, canon_primary.join(".git"));
    assert_eq!(wt_layout.objects, canon_primary.join(".git/objects"));
    assert_eq!(wt_layout.common_repo_root(), canon_primary.as_path());
    assert_eq!(
        aaf_fp::tree_fp_with_layout(&canon_wt, &wt_layout, TIMEOUT).unwrap(),
        aaf_fp::tree_fp(&canon_wt, TIMEOUT).unwrap()
    );

    let _ = Command::new("git")
        .args(["worktree", "remove", "--force", wt.to_str().unwrap()])
        .current_dir(primary)
        .output();
}
