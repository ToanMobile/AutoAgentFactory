#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for SEC-080: `aaf_fp::tree_fp` must not follow pre-planted
//! symlinks for temporary index files (`aaf-index-<pid>-<seq>`) or temporary
//! object directories (`aaf-objects-<pid>-<seq>`) inside `.git/`.

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(unix)]
const SEQ_RANGE: u64 = 32;
#[cfg(unix)]
const CANARY_CONTENT: &[u8] = b"CANARY-DO-NOT-CLOBBER\n";

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(["-c", "gc.auto=0", "-c", "maintenance.auto=false"])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn repo_with_commit_and_untracked() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    fs::write(dir.path().join("tracked.txt"), "committed content\n").unwrap();
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "init"]);
    fs::write(dir.path().join("untracked.txt"), "new untracked content\n").unwrap();
    dir
}

#[cfg(unix)]
#[test]
fn symlink_at_temp_index_does_not_clobber_target() {
    let repo = repo_with_commit_and_untracked();
    let outside = tempfile::tempdir().unwrap();
    let canary = outside.path().join("canary.txt");
    fs::write(&canary, CANARY_CONTENT).unwrap();

    let git_dir = repo.path().join(".git");
    let pid = std::process::id();
    for seq in 0..SEQ_RANGE {
        let link = git_dir.join(format!("aaf-index-{pid}-{seq}"));
        symlink(&canary, &link).unwrap();
    }

    let result = aaf_fp::tree_fp(repo.path(), TIMEOUT);
    let actual = fs::read(&canary).expect("canary file must still be readable");
    assert_eq!(
        actual,
        CANARY_CONTENT,
        "tree_fp (result={result:?}) clobbered external canary target via aaf-index symlink; \
         got {} bytes (lossy preview: {:?})",
        actual.len(),
        String::from_utf8_lossy(&actual[..actual.len().min(64)])
    );
}

#[cfg(unix)]
#[test]
fn symlink_at_temp_objects_does_not_write_outside() {
    let repo = repo_with_commit_and_untracked();
    let canary_dir = tempfile::tempdir().unwrap();

    let git_dir = repo.path().join(".git");
    let pid = std::process::id();
    for seq in 0..SEQ_RANGE {
        let link = git_dir.join(format!("aaf-objects-{pid}-{seq}"));
        symlink(canary_dir.path(), &link).unwrap();
    }

    let result = aaf_fp::tree_fp(repo.path(), TIMEOUT);
    assert!(
        canary_dir.path().is_dir(),
        "external canary directory must still exist after tree_fp (result={result:?})"
    );
    let mut entries: Vec<String> = fs::read_dir(canary_dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    assert!(
        entries.is_empty(),
        "tree_fp (result={result:?}) wrote objects into external canary directory via \
         aaf-objects symlink: {entries:?}"
    );
}

#[test]
fn normal_repo_without_symlinks_returns_git_tree_and_cleans_up() {
    let repo = repo_with_commit_and_untracked();

    let fp = aaf_fp::tree_fp(repo.path(), TIMEOUT).unwrap();
    assert!(
        fp.starts_with("git-tree:") && fp.len() > "git-tree:".len(),
        "unexpected fingerprint format: {fp:?}"
    );

    let leftovers: Vec<String> = fs::read_dir(repo.path().join(".git"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("aaf-index-") || name.starts_with("aaf-objects-"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "tree_fp left temporary entries in .git: {leftovers:?}"
    );
}
