#![allow(clippy::unwrap_used, clippy::expect_used)]
//! A13 O11-R9-1: `aaf gc` lists the task branch and snapshot namespaces once
//! per repository instead of spawning two git calls for every Done task ever
//! recorded (rows are never deleted, so that cost only grows), while the
//! report and what gets deleted stay the same.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use aaf_engine::db::Db;
use aaf_engine::gc::{GcOptions, run_gc};

#[path = "gc_support.rs"]
mod gc_support;
use gc_support::{done_task, git, has_branch, init_repo};

const CHILD_ENV: &str = "AAF_A13_FX2_GC_SPAWN_DIR";
const SNAP: &str = "refs/aaf/snapshots/";
/// Generous: the listing-based sweep needs about ten calls for this repo;
/// the per-task one needed two for each of the 51 Done tasks.
const MAX_CALLS: usize = 15;

fn write_exec(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn calls(log: &Path) -> Vec<String> {
    fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn has_ref(repo: &Path, name: &str) -> bool {
    !git(repo, &["for-each-ref", "--format=%(refname)", name])
        .trim()
        .is_empty()
}

/// A `git` wrapper early on `PATH` logs every call; the scenario runs in a
/// child test process because aaf resolves `git` once per process from the
/// `PATH` it started with.
#[test]
fn gc_git_calls_do_not_grow_with_done_tasks() {
    let dir = tempfile::tempdir().unwrap();
    let dir = fs::canonicalize(dir.path()).unwrap();
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    let real = aaf_fp::find_program(&path_var, "git", &[]).expect("git on PATH");
    write_exec(
        &dir.join("git"),
        &format!(
            "#!/bin/sh\necho \"$*\" >> '{log}'\nexec '{real}' \"$@\"\n",
            log = dir.join("calls.log").display(),
            real = real.display()
        ),
    );
    let mut paths = vec![dir.clone()];
    paths.extend(std::env::split_paths(&path_var));
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "gc_spawns_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env(CHILD_ENV, &dir)
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "child failed:\n{text}");
    assert!(text.contains("1 passed"), "child did not run:\n{text}");
}

#[test]
#[ignore = "run by gc_git_calls_do_not_grow_with_done_tasks in a child process"]
fn gc_spawns_child() {
    let Some(dir) = std::env::var_os(CHILD_ENV) else {
        return;
    };
    let log = Path::new(&dir).join("calls.log");
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let data = tmp.path().join("data");
    let worktrees = tmp.path().join("worktrees");
    init_repo(&repo);
    // Task `x` left a branch and two snapshot refs. The rest is not any Done
    // task's and stays: another task's refs sharing the prefix `x`, a branch
    // of a task gc does not know.
    git(&repo, &["branch", "aaf/x"]);
    for name in ["x/1", "x/2", "x-extra/1", "other/1"] {
        git(&repo, &["update-ref", &format!("{SNAP}{name}"), "HEAD"]);
    }
    git(&repo, &["branch", "aaf/x-extra"]);
    fs::create_dir_all(&data).unwrap();
    let db = Db::open(&data.join("aaf.db")).unwrap();
    for i in 0..50 {
        done_task(&db, &format!("t{i:02}"), &repo);
    }
    done_task(&db, "x", &repo);
    drop(db);

    let mut opts = GcOptions {
        repo_root: Some(repo.clone()),
        dry_run: true,
        ..GcOptions::default()
    };
    let before = calls(&log).len();
    let dry = run_gc(&data, &worktrees, &opts).unwrap();
    let dry_calls = calls(&log)[before..].to_vec();
    assert_eq!(dry.removed_branches, vec!["aaf/x".to_string()]);
    assert_eq!(dry.removed_snapshot_refs, 2);
    assert!(dry.warnings.is_empty(), "{:?}", dry.warnings);
    assert!(
        dry_calls.len() <= MAX_CALLS,
        "dry run spawned {} git calls:\n{}",
        dry_calls.len(),
        dry_calls.join("\n")
    );

    opts.dry_run = false;
    let before = calls(&log).len();
    let real = run_gc(&data, &worktrees, &opts).unwrap();
    let real_calls = calls(&log)[before..].to_vec();
    assert_eq!(real.removed_branches, vec!["aaf/x".to_string()]);
    assert_eq!(real.removed_snapshot_refs, 2);
    assert!(real.warnings.is_empty(), "{:?}", real.warnings);
    assert!(
        real_calls.len() <= MAX_CALLS,
        "real run spawned {} git calls:\n{}",
        real_calls.len(),
        real_calls.join("\n")
    );

    assert!(!has_branch(&repo, "aaf/x"));
    assert!(!has_ref(&repo, &format!("{SNAP}x/")));
    assert!(has_branch(&repo, "aaf/x-extra"));
    for name in ["x-extra/1", "other/1"] {
        assert!(has_ref(&repo, &format!("{SNAP}{name}")), "{name} deleted");
    }
}
