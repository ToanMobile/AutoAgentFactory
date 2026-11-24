#![allow(clippy::unwrap_used, clippy::expect_used)]
//! A13 L9-1: `aaf gc` must not sweep a merge-scratch worktree whose owning
//! process is still alive (verify runs before the merge lock is taken).

use std::fs;
use std::process::Command;

use aaf_engine::gc::{GcOptions, run_gc};

fn reaped_child_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

#[test]
fn gc_keeps_live_owner_merge_scratch_and_removes_dead_owner_orphan() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let worktrees = tmp.path().join("worktrees");
    let scratch = data.join("merge-scratch");
    fs::create_dir_all(&scratch).unwrap();

    let live = scratch.join(format!("0123456789abcdef-pr7-{}-0", std::process::id()));
    let orphan = scratch.join(format!("0123456789abcdef-pr8-{}-1", reaped_child_pid()));
    fs::create_dir_all(&live).unwrap();
    fs::create_dir_all(&orphan).unwrap();
    // Both carry an unlocked companion `<dir>.lock` (the crashed-scratch
    // signature); only the owner pid tells them apart.
    for dir in [&live, &orphan] {
        let mut lock = dir.as_os_str().to_os_string();
        lock.push(".lock");
        fs::write(lock, b"").unwrap();
    }

    run_gc(&data, &worktrees, &GcOptions::default()).unwrap();

    assert!(
        live.is_dir(),
        "scratch of a live owner (lock not held) was swept"
    );
    assert!(
        !orphan.exists(),
        "dead-owner orphan scratch must be removed"
    );
}
