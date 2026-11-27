#![allow(clippy::unwrap_used, clippy::expect_used)]
//! SEC-128 verification: host-side `git update-ref -d` calls (without
//! `--no-deref`) must not follow a symref planted in `<common>/refs/...` to
//! delete `refs/heads/main`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use aaf_engine::db::Db;
use aaf_engine::gc::{GcOptions, run_gc};
use aaf_engine::git::worktree::{SNAPSHOT_REF_PREFIX, delete_snapshot_refs};

#[path = "gc_support.rs"]
mod gc_support;
use gc_support::{done_task, git, init_repo};

const SCRUBBED_GIT_ENV: [&str; 6] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_PREFIX",
];

fn try_rev_parse(repo: &Path, rev: &str) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet", rev]);
    for var in SCRUBBED_GIT_ENV {
        cmd.env_remove(var);
    }
    let out = cmd.output().expect("spawn git");
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn common_git_dir(repo: &Path) -> PathBuf {
    let raw = git(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    );
    PathBuf::from(raw.trim())
}

fn write_symref(repo: &Path, refname: &str, target: &str) {
    let path = common_git_dir(repo).join(refname);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, format!("ref: {target}\n")).unwrap();
}

fn reaped_child_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

#[test]
fn gc_merge_ref_symref_to_main_does_not_delete_main() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let data = tmp.path().join("data");
    let worktrees = tmp.path().join("worktrees");
    init_repo(&repo);
    let main_before = try_rev_parse(&repo, "refs/heads/main").expect("main exists");

    let merge_ref = format!("refs/aaf-merge/pr-42-{}-0", reaped_child_pid());
    write_symref(&repo, &merge_ref, "refs/heads/main");

    let opts = GcOptions {
        repo_root: Some(repo.clone()),
        ..GcOptions::default()
    };
    let _ = run_gc(&data, &worktrees, &opts);

    let main_after = try_rev_parse(&repo, "refs/heads/main");
    assert_eq!(
        main_after.as_deref(),
        Some(main_before.as_str()),
        "SEC-128: gc deleting symref {merge_ref} must not delete refs/heads/main"
    );
}

#[test]
fn delete_snapshot_refs_symref_to_main_does_not_delete_main() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let main_before = try_rev_parse(&repo, "refs/heads/main").expect("main exists");

    let task_id = "sec128-snap";
    let snap_ref = format!("{SNAPSHOT_REF_PREFIX}{task_id}/1");
    write_symref(&repo, &snap_ref, "refs/heads/main");

    let _ = delete_snapshot_refs(&repo, task_id);

    let main_after = try_rev_parse(&repo, "refs/heads/main");
    assert_eq!(
        main_after.as_deref(),
        Some(main_before.as_str()),
        "SEC-128: delete_snapshot_refs on symref {snap_ref} must not delete refs/heads/main"
    );
}

#[test]
fn expected_oid_delete_follows_symref() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let data = tmp.path().join("data");
    let worktrees = tmp.path().join("worktrees");
    init_repo(&repo);
    let main_before = try_rev_parse(&repo, "refs/heads/main").expect("main exists");

    let task_id = "sec128-oid";
    let branch_ref = format!("refs/heads/aaf/{task_id}");
    write_symref(&repo, &branch_ref, "refs/heads/main");

    fs::create_dir_all(&data).unwrap();
    let db = Db::open(&data.join("aaf.db")).unwrap();
    done_task(&db, task_id, &repo);
    drop(db);

    let opts = GcOptions {
        repo_root: Some(repo.clone()),
        ..GcOptions::default()
    };
    let _ = run_gc(&data, &worktrees, &opts);

    let main_after = try_rev_parse(&repo, "refs/heads/main");
    assert_eq!(
        main_after.as_deref(),
        Some(main_before.as_str()),
        "SEC-128: gc deleting task branch symref {branch_ref} with expected oid \
         must not dereference and delete refs/heads/main"
    );
}

#[test]
fn gc_removes_plain_dead_merge_ref() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let data = tmp.path().join("data");
    let worktrees = tmp.path().join("worktrees");
    init_repo(&repo);
    let main_before = try_rev_parse(&repo, "refs/heads/main").expect("main exists");

    let merge_ref = format!("refs/aaf-merge/pr-42-{}-0", reaped_child_pid());
    git(&repo, &["update-ref", &merge_ref, &main_before]);
    assert_eq!(
        try_rev_parse(&repo, &merge_ref).as_deref(),
        Some(main_before.as_str())
    );

    let opts = GcOptions {
        repo_root: Some(repo.clone()),
        ..GcOptions::default()
    };
    let report = run_gc(&data, &worktrees, &opts).unwrap();

    assert_eq!(report.removed_merge_refs, 1);
    assert_eq!(
        try_rev_parse(&repo, &merge_ref),
        None,
        "plain dead-PID merge ref must be removed by gc"
    );
    assert_eq!(
        try_rev_parse(&repo, "refs/heads/main").as_deref(),
        Some(main_before.as_str()),
        "refs/heads/main must remain unchanged"
    );
}
