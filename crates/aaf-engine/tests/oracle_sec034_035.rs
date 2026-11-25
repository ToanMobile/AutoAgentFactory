#![allow(clippy::unwrap_used, clippy::expect_used)]
//! SEC-034 / SEC-035 oracle: short ref names resolve `refs/tags/*` before
//! `refs/heads/*` and `refs/remotes/*` (`gitrevisions(7)`). A tag an agent
//! creates (tags are shared by every worktree) must not:
//! - SEC-034: move the gate's explicit `gate.base_ref` (an empty change set
//!   would pass the head lock);
//! - SEC-035: fast-forward the checked-out local base branch after a merge.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use aaf_engine::gate::base as gate_base;
use aaf_engine::git::pr_merge::sync_local_base;
use tempfile::TempDir;

const SCRUBBED_GIT_ENV: [&str; 6] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_PREFIX",
];

fn git(dir: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    for var in SCRUBBED_GIT_ENV {
        cmd.env_remove(var);
    }
    let out = cmd.output().expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init(dir: &Path, bare: bool) {
    let mut args = vec!["init", "-q", "-b", "main"];
    if bare {
        args.push("--bare");
    }
    git(dir, &args);
    configure(dir);
}

fn configure(dir: &Path) {
    for (k, v) in [
        ("user.name", "aaf sec034 test"),
        ("user.email", "sec034@example.invalid"),
        ("commit.gpgsign", "false"),
        ("tag.gpgsign", "false"),
        ("core.hooksPath", "/dev/null"),
    ] {
        git(dir, &["config", k, v]);
    }
}

fn commit(dir: &Path, file: &str, body: &str) -> String {
    fs::write(dir.join(file), body).unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", file]);
    git(dir, &["rev-parse", "HEAD"])
}

/// Repo on a feature branch cut from `main` at `base`, one commit ahead.
/// Returns (tmp, base sha, head sha).
fn feature_repo() -> (TempDir, String, String) {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    init(repo, false);
    let base = commit(repo, "a.txt", "base\n");
    git(repo, &["checkout", "-q", "-b", "feat/x"]);
    let head = commit(repo, "b.txt", "work\n");
    (tmp, base, head)
}

#[test]
fn sec034_tag_named_like_explicit_base_branch_does_not_move_gate_base() {
    let (tmp, base, head) = feature_repo();
    let repo = tmp.path();
    // Agent plants a tag `main` on HEAD: merge-base(HEAD, HEAD) = HEAD would
    // empty the change set.
    git(repo, &["tag", "main", &head]);
    let resolved = gate_base::resolve(repo, "main", Duration::from_secs(15)).unwrap();
    assert_eq!(resolved.sha.as_deref(), Some(base.as_str()));
}

#[test]
fn sec034_tag_named_like_remote_tracking_base_does_not_move_gate_base() {
    let (tmp, base, head) = feature_repo();
    let repo = tmp.path();
    git(repo, &["update-ref", "refs/remotes/origin/main", &base]);
    git(repo, &["tag", "origin/main", &head]);
    let resolved = gate_base::resolve(repo, "origin/main", Duration::from_secs(15)).unwrap();
    assert_eq!(resolved.sha.as_deref(), Some(base.as_str()));
}

#[test]
fn sec034_name_that_only_exists_as_tag_is_rejected_unless_spelled_in_full() {
    let (tmp, base, _head) = feature_repo();
    let repo = tmp.path();
    git(repo, &["tag", "v1", &base]);
    let err = gate_base::resolve(repo, "v1", Duration::from_secs(15)).unwrap_err();
    assert!(err.to_string().contains("refs/tags/v1"), "{err}");
    let full = gate_base::resolve(repo, "refs/tags/v1", Duration::from_secs(15)).unwrap();
    assert_eq!(full.sha.as_deref(), Some(base.as_str()));
    // Plain revisions still work.
    let by_sha = gate_base::resolve(repo, &base, Duration::from_secs(15)).unwrap();
    assert_eq!(by_sha.sha.as_deref(), Some(base.as_str()));
}

#[test]
fn sec035_tag_shadowing_remote_base_does_not_fast_forward_checked_out_base() {
    let origin = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let other = TempDir::new().unwrap();
    init(origin.path(), true);
    let repo = work.path().join("r");
    git(
        work.path(),
        &[
            "clone",
            "-q",
            origin.path().to_str().unwrap(),
            repo.to_str().unwrap(),
        ],
    );
    configure(&repo);
    git(&repo, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    commit(&repo, "a.txt", "v1\n");
    git(&repo, &["push", "-q", "-u", "origin", "main"]);

    // Upstream advances main legitimately.
    let clone = other.path().join("c");
    git(
        other.path(),
        &[
            "clone",
            "-q",
            origin.path().to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    configure(&clone);
    let real_new = commit(&clone, "b.txt", "v2\n");
    git(&clone, &["push", "-q", "origin", "main"]);

    // Agent plants an evil descendant of local main and tags it `origin/main`.
    git(&repo, &["checkout", "-q", "-b", "evil"]);
    let evil = commit(&repo, "evil.txt", "pwn\n");
    git(&repo, &["checkout", "-q", "main"]);
    git(&repo, &["branch", "-q", "-D", "evil"]);
    git(&repo, &["tag", "origin/main", &evil]);

    let note = sync_local_base(&repo, "origin", "main");
    let now = git(&repo, &["rev-parse", "refs/heads/main"]);
    assert_ne!(now, evil, "local main fast-forwarded to the tag: {note}");
    assert_eq!(now, real_new, "{note}");
}
