#![allow(clippy::unwrap_used, clippy::expect_used)]
//! An agent can create a repository of its own inside the work tree it
//! edits (`sub/.git`, config included): the fingerprint must not start the
//! program its `core.fsmonitor` names.

use std::fs;
use std::path::Path;
use std::process::Command;

#[path = "exec_support.rs"]
mod exec_support;
use exec_support::write_executable;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

#[test]
fn the_fingerprint_runs_no_fsmonitor_of_an_embedded_repository() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let sub = repo.join("sub");
    fs::create_dir_all(&sub).unwrap();
    git(&repo, &["init", "--quiet"]);
    fs::write(repo.join("a.txt"), "a\n").unwrap();
    git(&sub, &["init", "--quiet"]);
    fs::write(sub.join("y.txt"), "y\n").unwrap();
    git(&sub, &["add", "-A"]);
    git(&sub, &["commit", "--quiet", "-m", "sub"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "--quiet", "-m", "init"]);
    let marker = dir.path().join("fsmonitor-ran");
    let monitor = dir.path().join("monitor.sh");
    write_executable(
        &monitor,
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
    );
    git(
        &sub,
        &["config", "core.fsmonitor", monitor.to_str().unwrap()],
    );
    fs::write(sub.join("y.txt"), "changed\n").unwrap();
    fs::write(repo.join("a.txt"), "b\n").unwrap();

    aaf_fp::tree_fp(&repo, aaf_fp::GIT_TIMEOUT).unwrap();
    aaf_fp::hidden_files(&repo, aaf_fp::GIT_TIMEOUT).unwrap();

    assert!(!marker.exists(), "the embedded repository's fsmonitor ran");
}
