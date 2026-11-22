#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "cli_support.rs"]
mod support;

use std::fs::{self, File};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use aaf_domain::ports::TaskStore;
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::executors::DEFAULT_WORKTREES_REL;
use aaf_engine::git::worktree;
use serde_json::json;
use support::{Sandbox, one_object};

fn spec() -> ImmutableSpec {
    ImmutableSpec {
        description: "CLI gc fixture task".to_string(),
        acceptance: vec!["passes".to_string()],
    }
}

fn insert_task(db: &Db, repo: &Path, id: &str, state: TaskState) {
    let mut task = Task::new_root(
        id,
        repo.to_string_lossy(),
        format!("Task {id}"),
        spec(),
        Priority::P2,
        1_700_000_000,
    );
    task.state = state;
    db.insert_task(&task).unwrap();
}

fn set_mtime_past(path: &Path, age: Duration) {
    let file = File::options().write(true).open(path).unwrap();
    let target = SystemTime::now().checked_sub(age).unwrap();
    file.set_modified(target).unwrap();
}

fn init_git_repo(dir: &Path, home: &Path) {
    fs::create_dir_all(dir).unwrap();
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["add", "-A"][..],
        &["commit", "-q", "-m", "init"][..],
    ] {
        if args[0] == "add" {
            fs::write(dir.join("README.md"), "repo2\n").unwrap();
        }
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=f@example.invalid",
            ])
            .args(args)
            .env("HOME", home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }
}

#[test]
fn gc_dry_run_json_previews_without_mutating() {
    let sb = Sandbox::new();
    let worktrees_dir = sb.home.join(DEFAULT_WORKTREES_REL);
    fs::create_dir_all(&sb.data).unwrap();
    let db = Db::open(&sb.data.join("aaf.db")).unwrap();
    insert_task(&db, &sb.repo, "t-done", TaskState::Done);

    let wt_done = worktree::create(&sb.repo, &worktrees_dir, "t-done", "main").unwrap();
    let wt_batch = worktree::create(&sb.repo, &worktrees_dir, "batch-leftover", "main").unwrap();

    let streams = sb.data.join("streams");
    fs::create_dir_all(&streams).unwrap();
    let old_stream = streams.join("old-run.jsonl");
    fs::write(&old_stream, "{\"event\":\"done\"}\n").unwrap();
    set_mtime_past(&old_stream, Duration::from_secs(20 * 86_400));

    let (code, out) = sb.json(&["gc", "--dry-run"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["ok"], json!(true));
    assert_eq!(out["command"], json!("gc"));
    assert_eq!(out["status"], json!("ok"));
    assert_eq!(out["dry_run"], json!(true));
    assert_eq!(out["removed_orphan_batches"], json!(["batch-leftover"]));
    assert_eq!(out["removed_worktrees"].as_array().unwrap().len(), 1);
    assert_eq!(out["removed_files"].as_array().unwrap().len(), 1);

    assert!(wt_done.path.is_dir(), "dry-run must not delete worktree");
    assert!(wt_batch.path.is_dir(), "dry-run must not delete batch");
    assert!(old_stream.is_file(), "dry-run must not delete stream");
}

#[test]
fn gc_refuses_inside_agent_run_with_exit_2() {
    let sb = Sandbox::new();
    let out = sb
        .command(&["gc", "--dry-run", "--json"])
        .env("AAF_RUN_ID", "run-inside-agent")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let body = one_object(&out);
    assert_eq!(body["ok"], json!(false));
    assert_eq!(body["command"], json!("gc"));
    assert_eq!(body["status"], json!("refused"));
}

#[test]
fn gc_all_cleans_multiple_recorded_repos_and_honors_cancelled_force_flags() {
    let sb = Sandbox::new();
    let worktrees_dir = sb.home.join(DEFAULT_WORKTREES_REL);
    let repo2 = sb.repo.parent().unwrap().join("repo2");
    init_git_repo(&repo2, &sb.home);
    let repo2 = fs::canonicalize(&repo2).unwrap();

    fs::create_dir_all(&sb.data).unwrap();
    let db = Db::open(&sb.data.join("aaf.db")).unwrap();
    insert_task(&db, &sb.repo, "t-done-1", TaskState::Done);
    insert_task(&db, &repo2, "t-done-2", TaskState::Done);
    insert_task(&db, &repo2, "t-canc-dirty", TaskState::Cancelled);

    let wt1 = worktree::create(&sb.repo, &worktrees_dir, "t-done-1", "main").unwrap();
    let wt2 = worktree::create(&repo2, &worktrees_dir, "t-done-2", "main").unwrap();
    let wt_dirty = worktree::create(&repo2, &worktrees_dir, "t-canc-dirty", "main").unwrap();
    fs::write(wt_dirty.path.join("dirty.txt"), "keep\n").unwrap();

    let (code, out) = sb.json(&["gc", "--all", "--include-cancelled"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["ok"], json!(true));
    assert_eq!(out["repos_scanned"].as_array().unwrap().len(), 2);
    assert!(!wt1.path.exists());
    assert!(!wt2.path.exists());
    assert_eq!(out["skipped_dirty"], json!(["t-canc-dirty"]));
    assert!(wt_dirty.path.join("dirty.txt").is_file());

    let (code2, out2) = sb.json(&["gc", "--all", "--include-cancelled", "--force"]);
    assert_eq!(code2, 0, "{out2}");
    assert_eq!(out2["ok"], json!(true));
    assert!(!wt_dirty.path.exists());
}
