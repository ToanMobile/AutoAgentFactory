#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for:
//! 1. Claude CLI OAuth/Keychain refresh errors inside Seatbelt sandbox
//!    (`Failed to refresh OAuth token: can't save to the Keychain`) classified
//!    as `InfraKind::Auth` so the factory rotates accounts instead of burning
//!    cycles as `stuck`, with zero false positives on normal non-zero-token runs.
//! 2. `aaf gc --reap-stale` safely reaping zombie `Running` runs and returning
//!    zombie `Running` tasks to `Ready` on idle repos (while skipping busy repos
//!    holding `.run.lock` and honoring `--dry-run`).

use std::fs;
use std::process::Command;

use aaf_domain::ports::{InfraKind, RunStore, TaskStore};
use aaf_domain::run::{RunRecord, RunStatus};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::{Db, private};
use aaf_engine::executors::claude::parse_stream;
use aaf_engine::gc::{GcOptions, run_gc};

#[test]
fn claude_oauth_keychain_error_with_zero_tokens_is_classified_as_auth_infra() {
    // Exact shape observed in real macOS Seatbelt sandbox runs: Claude CLI emits
    // subtype="success" (is_error=false) with 0/0 usage and the Keychain error text.
    let transcript = concat!(
        r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"Failed to refresh OAuth token: can't save to the Keychain. If locked, run `security unlock-keychain`. If sandboxed, use `claude setup-token`. Otherwise, retry, then `claude auth login`"}]}}"#,
        "\n",
        r#"{"type":"result","subtype":"success","is_error":false,"result":"Failed to refresh OAuth token: can't save to the Keychain. If locked, run `security unlock-keychain`. If sandboxed, use `claude setup-token`. Otherwise, retry, then `claude auth login`","usage":{"input_tokens":0,"output_tokens":0}}"#,
        "\n",
    );
    let summary = parse_stream(transcript);
    let infra = summary
        .infra
        .expect("OAuth Keychain refresh failure must classify as InfraFailure");
    assert_eq!(infra.kind, InfraKind::Auth);
    assert!(infra.raw.contains("Keychain"), "{}", infra.raw);
}

#[test]
fn claude_oauth_keychain_error_in_truncated_assistant_only_stream_is_auth_infra() {
    let transcript = r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"Failed to refresh OAuth token: can't save to the Keychain."}]}}"#;
    let summary = parse_stream(transcript);
    let infra = summary
        .infra
        .expect("truncated zero-token OAuth Keychain stream must classify as InfraFailure");
    assert_eq!(infra.kind, InfraKind::Auth);
}

#[test]
fn normal_completed_run_mentioning_oauth_prose_is_not_classified_as_auth_infra() {
    // A real LLM turn has non-zero tokens and is_error=false: prose mentioning
    // OAuth/Keychain (e.g. fixing this very bug!) must never false-positive.
    let transcript = concat!(
        r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"Fixed handling for Failed to refresh OAuth token: can't save to the Keychain."}],"usage":{"input_tokens":1200,"output_tokens":180}}}"#,
        "\n",
        r#"{"type":"result","subtype":"success","is_error":false,"result":"Fixed handling for Failed to refresh OAuth token: can't save to the Keychain.","usage":{"input_tokens":1200,"output_tokens":180}}"#,
        "\n",
    );
    let summary = parse_stream(transcript);
    assert!(
        summary.infra.is_none(),
        "non-zero-token success must not classify as auth infra: {:?}",
        summary.infra
    );
}

fn init_git_repo(dir: &std::path::Path) {
    let run = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {:?}", out);
    };
    run(&["init", "-b", "main"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "Test"]);
    fs::write(dir.join("README.md"), "hi\n").unwrap();
    run(&["add", "README.md"]);
    run(&["commit", "-m", "init"]);
}

fn make_task(id: &str, repo: &str, state: TaskState) -> Task {
    let mut t = Task::new_root(
        id,
        repo,
        format!("Task {id}"),
        ImmutableSpec {
            description: "desc".into(),
            acceptance: vec!["ac".into()],
        },
        Priority::P2,
        1_700_000_000,
    );
    t.state = state;
    t
}

#[test]
fn gc_reap_stale_recovers_idle_repo_and_skips_busy_repo_and_respects_dry_run() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let worktrees = tmp.path().join("worktrees");
    let idle_repo = tmp.path().join("idle-repo");
    let busy_repo = tmp.path().join("busy-repo");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&worktrees).unwrap();
    fs::create_dir_all(&idle_repo).unwrap();
    fs::create_dir_all(&busy_repo).unwrap();
    init_git_repo(&idle_repo);
    init_git_repo(&busy_repo);

    let idle_str = fs::canonicalize(&idle_repo)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let busy_str = fs::canonicalize(&busy_repo)
        .unwrap()
        .to_string_lossy()
        .into_owned();

    let db = Db::open(&data.join("aaf.db")).unwrap();
    db.insert_task(&make_task("t-idle-run", &idle_str, TaskState::Running))
        .unwrap();
    db.insert_task(&make_task("t-idle-gate", &idle_str, TaskState::Gating))
        .unwrap();
    db.insert_task(&make_task("t-busy-run", &busy_str, TaskState::Running))
        .unwrap();

    for (run_id, task_id) in [
        ("run-idle-1", "t-idle-run"),
        ("run-idle-2", "t-idle-gate"),
        ("run-busy-1", "t-busy-run"),
    ] {
        db.insert_run(&RunRecord {
            id: run_id.into(),
            task_id: task_id.into(),
            account_id: None,
            executor: "claude".into(),
            worktree_path: String::new(),
            branch: String::new(),
            status: RunStatus::Running,
            exit_code: None,
            tokens_in: 120,
            tokens_out: 30,
            started_at: 1_700_000_000,
            ended_at: None,
        })
        .unwrap();
    }
    drop(db);

    // Hold `.run.lock` on `busy_repo` so GC must skip it.
    let locks_dir = data.join("locks");
    private::create_dir(&locks_dir).unwrap();
    let busy_id = aaf_fp::repo_id(&fs::canonicalize(&busy_repo).unwrap());
    let lock_file = private::open_file(&locks_dir.join(format!("{busy_id}.run.lock"))).unwrap();
    lock_file.try_lock().unwrap();

    // 1. Dry-run previews reaps without mutating DB.
    let dry = run_gc(
        &data,
        &worktrees,
        &GcOptions {
            all: true,
            dry_run: true,
            reap_stale: true,
            ..GcOptions::default()
        },
    )
    .unwrap();
    assert_eq!(dry.skipped_busy.len(), 1);
    assert_eq!(dry.reaped_stale_runs.len(), 2);
    assert_eq!(dry.recovered_running_tasks, vec!["t-idle-run"]);
    let db = Db::open_read_only(&data.join("aaf.db")).unwrap();
    assert_eq!(db.get_task("t-idle-run").unwrap().state, TaskState::Running);
    assert_eq!(db.running_run_ids().unwrap().len(), 3);
    drop(db);

    // 2. Real run reaps idle repo's stale runs & recovers its Running task (keeping Gating task Gating),
    //    while leaving busy repo completely untouched.
    let real = run_gc(
        &data,
        &worktrees,
        &GcOptions {
            all: true,
            dry_run: false,
            reap_stale: true,
            ..GcOptions::default()
        },
    )
    .unwrap();
    assert_eq!(real.skipped_busy.len(), 1);
    assert_eq!(real.reaped_stale_runs.len(), 2);
    assert_eq!(real.recovered_running_tasks, vec!["t-idle-run"]);

    let db = Db::open_read_only(&data.join("aaf.db")).unwrap();
    assert_eq!(db.get_task("t-idle-run").unwrap().state, TaskState::Ready);
    assert_eq!(db.get_task("t-idle-gate").unwrap().state, TaskState::Gating);
    assert_eq!(db.get_task("t-busy-run").unwrap().state, TaskState::Running);
    let still_running = db.running_run_ids().unwrap();
    assert_eq!(still_running.len(), 1);
    assert!(still_running.contains("run-busy-1"));
    let reaped_run = db
        .runs_for_task("t-idle-run")
        .unwrap()
        .into_iter()
        .find(|r| r.id == "run-idle-1")
        .unwrap();
    assert_eq!(reaped_run.status, RunStatus::Killed);
    assert_eq!(reaped_run.tokens_in, 120);
    assert_eq!(reaped_run.tokens_out, 30);

    lock_file.unlock().unwrap();
}

#[test]
fn gc_reap_stale_also_reaps_zombie_runs_of_deleted_repo_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let worktrees = tmp.path().join("worktrees");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&worktrees).unwrap();
    let gone_repo = tmp
        .path()
        .join("deleted-repo")
        .to_string_lossy()
        .into_owned();

    let db = Db::open(&data.join("aaf.db")).unwrap();
    db.insert_task(&make_task("t-gone", &gone_repo, TaskState::Running))
        .unwrap();
    db.insert_run(&RunRecord {
        id: "run-gone-1".into(),
        task_id: "t-gone".into(),
        account_id: None,
        executor: "claude".into(),
        worktree_path: String::new(),
        branch: String::new(),
        status: RunStatus::Running,
        exit_code: None,
        tokens_in: 0,
        tokens_out: 0,
        started_at: 1_700_000_000,
        ended_at: None,
    })
    .unwrap();
    drop(db);

    let rep = run_gc(
        &data,
        &worktrees,
        &GcOptions {
            all: true,
            dry_run: false,
            reap_stale: true,
            ..GcOptions::default()
        },
    )
    .unwrap();
    assert_eq!(rep.reaped_stale_runs, vec!["run-gone-1"]);
    assert_eq!(rep.recovered_running_tasks, vec!["t-gone"]);
    let db = Db::open_read_only(&data.join("aaf.db")).unwrap();
    assert_eq!(db.get_task("t-gone").unwrap().state, TaskState::Ready);
    assert!(db.running_run_ids().unwrap().is_empty());
}
