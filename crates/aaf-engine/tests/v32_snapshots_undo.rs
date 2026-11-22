#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Integration tests for Phase 3 pre-turn snapshots, locked rollback (`undo`),
//! and spec-preserving human task steering (`steer`).

#[path = "git_support.rs"]
mod support;

use std::fs;
use std::path::Path;

use aaf_domain::event::EventKind;
use aaf_domain::ports::{EventSink, TaskStore};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::{Db, private};
use aaf_engine::factory::snapshots::{
    create_turn_snapshot_ref, preview_latest_undo, steer_task, undo_latest_turn,
};
use aaf_engine::factory::{GitOps, RealGit, resolve_base};
use aaf_engine::git::worktree;
use serde_json::Value;
use support::{Fixture, commit_file, git};

fn test_spec() -> ImmutableSpec {
    ImmutableSpec {
        description: "Original immutable spec".to_string(),
        acceptance: vec!["criterion 1".to_string()],
    }
}

fn insert_task(db: &Db, repo: &Path, id: &str, state: TaskState) -> Task {
    let mut task = Task::new_root(
        id,
        repo.to_string_lossy(),
        format!("Task {id}"),
        test_spec(),
        Priority::P2,
        1_700_000_000,
    );
    task.state = state;
    db.insert_task(&task).unwrap();
    task
}

#[test]
fn snapshot_ref_and_db_round_trip() {
    let fx = Fixture::new();
    let data = fx.repo.parent().unwrap().join("data");
    fs::create_dir_all(&data).unwrap();
    let db = Db::open(&data.join("aaf.db")).unwrap();
    insert_task(&db, &fx.repo, "t-snap1", TaskState::Ready);

    let wt = worktree::create(&fx.repo, &fx.worktrees, "t-snap1", "main").unwrap();
    let head = git(&wt.path, &["rev-parse", "HEAD"]);

    let (ref_name, oid) =
        create_turn_snapshot_ref(&fx.repo, &wt.path, "t-snap1", "run-abc", 1_700_000_042).unwrap();
    assert_eq!(oid, head);
    assert_eq!(
        ref_name,
        "refs/aaf/snapshots/t-snap1/00000000001700000042-run-abc"
    );
    assert_eq!(git(&fx.repo, &["rev-parse", "--verify", &ref_name]), head);

    let id = db
        .insert_snapshot("t-snap1", "run-abc", &ref_name, &oid, 1_700_000_042)
        .unwrap();
    let latest = db.latest_snapshot("t-snap1").unwrap().unwrap();
    assert_eq!(latest.id, id);
    assert_eq!(latest.task_id, "t-snap1");
    assert_eq!(latest.run_id, "run-abc");
    assert_eq!(latest.ref_name, ref_name);
    assert_eq!(latest.commit_oid, oid);
    assert_eq!(latest.created_at, 1_700_000_042);

    let listed = db.list_snapshots("t-snap1").unwrap();
    assert_eq!(listed, vec![latest]);

    // RealGit::create_turn_snapshot also writes a snapshot ref.
    let base = resolve_base(&fx.repo, "auto").unwrap();
    let real_git = RealGit::new(
        fx.repo.clone(),
        fx.worktrees.clone(),
        data.join("locks"),
        base,
    );
    let (ref2, oid2) = real_git
        .create_turn_snapshot(&wt, "run-def", 1_700_000_099)
        .unwrap()
        .unwrap();
    assert_eq!(oid2, head);
    assert!(ref2.starts_with("refs/aaf/snapshots/t-snap1/"));
    assert_eq!(git(&fx.repo, &["rev-parse", "--verify", &ref2]), head);

    assert!(db.delete_snapshot(id).unwrap());
    assert!(!db.delete_snapshot(id).unwrap());
    assert_eq!(db.latest_snapshot("t-snap1").unwrap(), None);
}

#[test]
fn undo_fails_closed_when_run_lock_is_held() {
    let fx = Fixture::new();
    let data = fx.repo.parent().unwrap().join("data");
    fs::create_dir_all(&data).unwrap();
    let db = Db::open(&data.join("aaf.db")).unwrap();
    insert_task(&db, &fx.repo, "t-lock", TaskState::Review);

    let wt = worktree::create(&fx.repo, &fx.worktrees, "t-lock", "main").unwrap();
    let (ref_name, oid) =
        create_turn_snapshot_ref(&fx.repo, &wt.path, "t-lock", "run-1", 100).unwrap();
    db.insert_snapshot("t-lock", "run-1", &ref_name, &oid, 100)
        .unwrap();

    // Hold `<data>/locks/<repo_id>.run.lock` exclusively.
    let locks_dir = data.join("locks");
    private::create_dir(&locks_dir).unwrap();
    let lock_path = locks_dir.join(format!("{}.run.lock", aaf_fp::repo_id(&fx.repo)));
    let held = private::open_file(&lock_path).unwrap();
    held.try_lock().unwrap();

    let err = undo_latest_turn(&db, &data, &fx.repo, &fx.worktrees, "t-lock", 200).unwrap_err();
    assert!(
        err.contains("run lock"),
        "expected run lock error, got: {err}"
    );
    assert_eq!(db.get_task("t-lock").unwrap().state, TaskState::Review);
    assert!(db.latest_snapshot("t-lock").unwrap().is_some());
    drop(held);
}

#[test]
fn undo_fails_closed_on_tampered_or_missing_snapshot_ref() {
    let fx = Fixture::new();
    let data = fx.repo.parent().unwrap().join("data");
    fs::create_dir_all(&data).unwrap();
    let db = Db::open(&data.join("aaf.db")).unwrap();
    insert_task(&db, &fx.repo, "t-tamper", TaskState::Review);

    let wt = worktree::create(&fx.repo, &fx.worktrees, "t-tamper", "main").unwrap();
    let (ref_name, oid) =
        create_turn_snapshot_ref(&fx.repo, &wt.path, "t-tamper", "run-1", 100).unwrap();
    db.insert_snapshot("t-tamper", "run-1", &ref_name, &oid, 100)
        .unwrap();

    // Advance branch and tamper the snapshot ref to point to the new commit.
    commit_file(&wt.path, "changed.txt", "turn1\n", "turn 1");
    let new_head = git(&wt.path, &["rev-parse", "HEAD"]);
    assert_ne!(new_head, oid);
    git(&fx.repo, &["update-ref", &ref_name, &new_head]);

    let err = undo_latest_turn(&db, &data, &fx.repo, &fx.worktrees, "t-tamper", 200).unwrap_err();
    assert!(
        err.contains("mismatch"),
        "expected mismatch error, got: {err}"
    );
    assert_eq!(git(&wt.path, &["rev-parse", "HEAD"]), new_head);

    // Deleting the git ref must also fail closed.
    git(&fx.repo, &["update-ref", "-d", &ref_name]);
    let missing_err = preview_latest_undo(&db, &fx.repo, "t-tamper").unwrap_err();
    assert!(
        missing_err.contains("missing") || missing_err.contains("unreadable"),
        "expected missing ref error, got: {missing_err}"
    );
}

#[test]
fn undo_refuses_forbidden_task_states() {
    let fx = Fixture::new();
    let data = fx.repo.parent().unwrap().join("data");
    fs::create_dir_all(&data).unwrap();
    let db = Db::open(&data.join("aaf.db")).unwrap();

    for (idx, state) in [
        TaskState::Running,
        TaskState::Gating,
        TaskState::Done,
        TaskState::Cancelled,
        TaskState::Backlog,
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("t-forbid-{idx}");
        insert_task(&db, &fx.repo, &id, state);
        let wt = worktree::create(&fx.repo, &fx.worktrees, &id, "main").unwrap();
        let (ref_name, oid) =
            create_turn_snapshot_ref(&fx.repo, &wt.path, &id, "run-1", 100).unwrap();
        db.insert_snapshot(&id, "run-1", &ref_name, &oid, 100)
            .unwrap();

        let err = undo_latest_turn(&db, &data, &fx.repo, &fx.worktrees, &id, 200).unwrap_err();
        assert!(
            err.contains(state.as_str()),
            "expected error mentioning state {}, got: {err}",
            state.as_str()
        );
        assert_eq!(db.get_task(&id).unwrap().state, state);
    }
}

#[test]
fn undo_on_review_and_blocked_resets_cleans_receipts_and_moves_ready() {
    for (id, start_state) in [
        ("t-review", TaskState::Review),
        ("t-blocked", TaskState::Blocked),
        ("t-ready", TaskState::Ready),
    ] {
        let fx = Fixture::new();
        let data = fx.repo.parent().unwrap().join("data");
        fs::create_dir_all(&data).unwrap();
        let db = Db::open(&data.join("aaf.db")).unwrap();
        insert_task(&db, &fx.repo, id, start_state);

        let wt = worktree::create(&fx.repo, &fx.worktrees, id, "main").unwrap();
        let (ref_name, snap_oid) =
            create_turn_snapshot_ref(&fx.repo, &wt.path, id, "run-7", 500).unwrap();
        db.insert_snapshot(id, "run-7", &ref_name, &snap_oid, 500)
            .unwrap();

        // Simulate an agent turn: committed change + untracked file + stale receipt/approval/audit.
        commit_file(&wt.path, "agent.rs", "fn bad() {}\n", "agent turn");
        fs::write(wt.path.join("untracked.tmp"), "junk").unwrap();

        let receipt = aaf_fp::receipt_path(&data, &wt.path);
        fs::create_dir_all(receipt.parent().unwrap()).unwrap();
        fs::write(&receipt, "{}").unwrap();
        let rid = aaf_fp::repo_id(&wt.path);
        let approval_file = data.join("approvals").join(&rid).join("lock.json");
        fs::create_dir_all(approval_file.parent().unwrap()).unwrap();
        fs::write(&approval_file, "{}").unwrap();
        let audit_file = data.join("audits").join(&rid).join("verdict.audit.json");
        fs::create_dir_all(audit_file.parent().unwrap()).unwrap();
        fs::write(&audit_file, "{}").unwrap();

        let undone = undo_latest_turn(&db, &data, &fx.repo, &fx.worktrees, id, 600).unwrap();
        assert_eq!(undone.commit_oid, snap_oid);
        assert_eq!(undone.ref_name, ref_name);

        // Worktree HEAD rolled back and untracked files removed.
        assert_eq!(git(&wt.path, &["rev-parse", "HEAD"]), snap_oid);
        assert!(!wt.path.join("agent.rs").exists());
        assert!(!wt.path.join("untracked.tmp").exists());

        // Snapshot ref and DB row removed.
        let ref_exists = std::process::Command::new("git")
            .arg("-C")
            .arg(&fx.repo)
            .args(["rev-parse", "--verify", "--quiet", &ref_name])
            .status()
            .unwrap()
            .success();
        assert!(!ref_exists, "snapshot ref {ref_name} should be deleted");
        assert_eq!(db.latest_snapshot(id).unwrap(), None);

        // Stale receipts, approvals, and audits cleared.
        assert!(!receipt.exists());
        assert!(!approval_file.exists());
        assert!(!audit_file.exists());

        // Task state is Ready and undo event was emitted.
        assert_eq!(db.get_task(id).unwrap().state, TaskState::Ready);
        let events = db.events_for_task(id, 10).unwrap();
        let undo_ev = events
            .iter()
            .find(|e| e.kind == EventKind::TaskStateChanged)
            .expect("expected TaskStateChanged event");
        let payload: Value = serde_json::from_str(&undo_ev.payload_json).unwrap();
        assert_eq!(payload["to"], "ready");
        assert_eq!(payload["why"], "undo");
        assert_eq!(payload["commit_oid"], snap_oid);
        assert_eq!(payload["ref_name"], ref_name);
    }
}

#[test]
fn steer_appends_note_preserves_spec_and_truncates_safely() {
    let fx = Fixture::new();
    let data = fx.repo.parent().unwrap().join("data");
    fs::create_dir_all(&data).unwrap();
    let db = Db::open(&data.join("aaf.db")).unwrap();
    let original = insert_task(&db, &fx.repo, "t-steer", TaskState::Ready);

    // Reject empty / control-only notes.
    assert!(steer_task(&db, "t-steer", "   \n\t  ", 100).is_err());

    // Appends sanitized note without modifying ImmutableSpec.
    let stored = steer_task(&db, "t-steer", "use\tstrict\nvalidation", 101).unwrap();
    assert_eq!(stored, "steer: use strict validation");
    let after = db.get_task("t-steer").unwrap();
    assert_eq!(after.spec, original.spec);
    assert_eq!(after.context_notes, vec!["steer: use strict validation"]);

    // Multibyte UTF-8 longer than 2000 bytes truncates cleanly on char boundary.
    let long_utf8 = "ế".repeat(900); // 3 bytes * 900 = 2700 bytes
    let stored_long = steer_task(&db, "t-steer", &long_utf8, 102).unwrap();
    let body = stored_long.strip_prefix("steer: ").unwrap();
    assert!(body.len() <= 2000);
    assert!(body.len() >= 1998);
}
