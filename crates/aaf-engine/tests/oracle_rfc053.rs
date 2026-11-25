#![allow(clippy::unwrap_used, clippy::expect_used)]
//! RFC-053 oracle tests:
//! (a) Schema v5 -> v6 migration, conditional `events_no_delete`, `idx_events_kind_created`,
//!     and read-only compatibility on v5/v6.
//! (b) Crash-safe JSONL archival of old terminal-task events (`archive_old_terminal_events`).
//! (c) Paired `.png` + `.proof.json` retention (`clean_expired_proofs`) with receipt protection,
//!     mirror cleanup, symlink skipping, dry-run, and run-lock guard.
//! (d) `lightweight_sweep` audit cleanup when DB is healthy and strict skip when DB is corrupt,
//!     without ever mutating `aaf.db`.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::gate::{
    GateMode, GateStatus, HeadLockReport, LockStatus, RECEIPT_SCHEMA_VERSION, Receipt,
    StoreComplianceReport,
};
use aaf_domain::ports::{EventSink, TaskStore};
use aaf_domain::proof::{PROOF_SCHEMA_VERSION, ProofRecord};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::{Db, SCHEMA_VERSION, private};
use aaf_engine::gc::{
    archive_old_terminal_events, clean_expired_proofs, collect_protected_tree_fps,
    lightweight_sweep,
};
use aaf_engine::git::worktree;
use aaf_engine::proof::{HOST_CAPTURE_DEVICE, HOST_CAPTURE_TOOL, captured_proof_image_path};
use rusqlite::Connection;
use serde_json::Value;

const V5_SCHEMA: &str = "\
CREATE TABLE tasks (id TEXT PRIMARY KEY, repo_root TEXT NOT NULL, parent_id TEXT REFERENCES tasks(id), root_id TEXT NOT NULL, depth INTEGER NOT NULL, title TEXT NOT NULL, spec TEXT NOT NULL, context_notes TEXT NOT NULL, state TEXT NOT NULL, priority TEXT NOT NULL, fingerprint TEXT, blocked_reason TEXT, unblock_cycles INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, scope_paths TEXT NOT NULL DEFAULT '[]', retry_after INTEGER, auto_recover_count INTEGER NOT NULL DEFAULT 0);
CREATE TABLE root_budgets (root_id TEXT PRIMARY KEY, tokens_used INTEGER NOT NULL, token_limit INTEGER NOT NULL, wall_secs_used INTEGER NOT NULL, wall_secs_limit INTEGER NOT NULL, cycles_used INTEGER NOT NULL, cycles_limit INTEGER NOT NULL, frozen INTEGER NOT NULL);
CREATE TABLE task_deps (task_id TEXT NOT NULL REFERENCES tasks(id), blocked_by TEXT NOT NULL REFERENCES tasks(id), PRIMARY KEY (task_id, blocked_by));
CREATE TABLE runs (id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES tasks(id), account_id TEXT, executor TEXT NOT NULL, worktree_path TEXT NOT NULL, branch TEXT NOT NULL, status TEXT NOT NULL, exit_code INTEGER, tokens_in INTEGER NOT NULL, tokens_out INTEGER NOT NULL, started_at INTEGER NOT NULL, ended_at INTEGER);
CREATE TABLE accounts (id TEXT PRIMARY KEY, provider TEXT NOT NULL, config_dir TEXT NOT NULL, cooldown_until INTEGER, name TEXT NOT NULL DEFAULT '');
CREATE TABLE events (id INTEGER PRIMARY KEY AUTOINCREMENT, task_id TEXT, run_id TEXT, kind TEXT NOT NULL, payload_json TEXT NOT NULL, created_at INTEGER NOT NULL);
CREATE TABLE closed_fingerprints (id INTEGER PRIMARY KEY, fingerprint TEXT NOT NULL, repo_root TEXT NOT NULL, task_id TEXT NOT NULL, closed_at INTEGER NOT NULL);
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE turn_snapshots (id INTEGER PRIMARY KEY AUTOINCREMENT, task_id TEXT NOT NULL REFERENCES tasks(id), run_id TEXT NOT NULL, ref_name TEXT NOT NULL, commit_oid TEXT NOT NULL, created_at INTEGER NOT NULL);
CREATE TRIGGER events_no_update BEFORE UPDATE ON events BEGIN SELECT RAISE(ABORT, 'events is append-only'); END;
CREATE TRIGGER events_no_delete BEFORE DELETE ON events BEGIN SELECT RAISE(ABORT, 'events is append-only'); END;
PRAGMA user_version = 5;";

fn sample_task(id: &str, repo: &str, state: TaskState) -> Task {
    let mut task = Task::new_root(
        id,
        repo,
        format!("task {id}"),
        ImmutableSpec {
            description: "rfc-053 test".into(),
            acceptance: vec!["ok".into()],
        },
        Priority::P1,
        1_700_000_000,
    );
    task.state = state;
    task
}

fn init_git_repo(dir: &Path) {
    let run = |args: &[&str]| {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(dir)
                .status()
                .unwrap()
                .success()
        );
    };
    run(&["init", "-q", "-b", "main"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "Test"]);
    fs::write(dir.join("README.md"), "# demo\n").unwrap();
    run(&["add", "."]);
    run(&["commit", "-q", "-m", "init"]);
}

fn set_mtime_past(path: &Path, ago: Duration) {
    let file = fs::File::options().write(true).open(path).unwrap();
    file.set_modified(SystemTime::now() - ago).unwrap();
}

fn count_events(db_path: &Path, where_clause: &str) -> i64 {
    let conn = Connection::open(db_path).unwrap();
    let sql = format!("SELECT COUNT(*) FROM events {where_clause}");
    conn.query_row(&sql, [], |r| r.get(0)).unwrap()
}

fn write_proof_pair(data_dir: &Path, repo_root: &Path, tree_fp: &str) -> (PathBuf, PathBuf) {
    let png_path = captured_proof_image_path(data_dir, repo_root, tree_fp);
    fs::create_dir_all(png_path.parent().unwrap()).unwrap();
    let bytes = b"\x89PNG\r\n\x1a\nfake-png-payload-for-gc-test";
    fs::write(&png_path, bytes).unwrap();
    let canon_png = fs::canonicalize(&png_path).unwrap();
    let record = ProofRecord {
        schema_version: PROOF_SCHEMA_VERSION,
        repo_root: repo_root.to_string_lossy().into_owned(),
        tree_fp: tree_fp.to_string(),
        image_path: canon_png.to_string_lossy().into_owned(),
        image_sha256: aaf_fp::sha256_hex(bytes),
        width: 100,
        height: 100,
        byte_len: bytes.len() as u64,
        source_tool: HOST_CAPTURE_TOOL.to_string(),
        device_serial: HOST_CAPTURE_DEVICE.to_string(),
        task_id: None,
        recorded_at: 1_700_000_000,
        base_sha: None,
        flows: Vec::new(),
    };
    (png_path, aaf_fp::write_proof(data_dir, &record).unwrap())
}

fn write_receipt_for(data_dir: &Path, repo_root: &Path, tree_fp: &str) {
    let key = aaf_fp::load_or_create_key(data_dir).unwrap();
    let receipt = Receipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        repo_root: repo_root.to_string_lossy().into_owned(),
        tree_fp: tree_fp.to_string(),
        head_sha: Some("1".repeat(40)),
        base_sha: Some("0".repeat(40)),
        mode: GateMode::Scoped,
        status: GateStatus::Clear,
        reasons: vec![],
        config_hash: "cfg-053".into(),
        commands: vec![],
        static_findings: vec![],
        suppressions: 0,
        head_lock: HeadLockReport {
            status: LockStatus::Clear,
            lock_fp: String::new(),
            locked_changes: vec![],
            approval: None,
        },
        store_compliance: StoreComplianceReport::not_applicable(),
        created_at: 1_700_000_100,
    };
    aaf_fp::write_receipt(data_dir, &key, &receipt).unwrap();
}

#[test]
fn schema_v5_to_v6_conditional_delete_trigger_and_index() {
    assert_eq!(SCHEMA_VERSION, 6);
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("aaf.db");

    {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(V5_SCHEMA).unwrap();
        for (id, state) in [
            ("t-backlog", "backlog"),
            ("t-ready", "ready"),
            ("t-running", "running"),
            ("t-gating", "gating"),
            ("t-review", "review"),
            ("t-blocked", "blocked"),
            ("t-done", "done"),
            ("t-cancelled", "cancelled"),
        ] {
            conn.execute(
                "INSERT INTO tasks VALUES (?1, '/repo', NULL, ?1, 0, ?1, \
                 '{\"description\":\"d\",\"acceptance\":[\"a\"]}', '[]', ?2, 'P1', NULL, NULL, 0, 10, 10, '[]', NULL, 0)",
                rusqlite::params![id, state],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO events (task_id, run_id, kind, payload_json, created_at) VALUES (?1, NULL, 'task_created', '{}', 100)",
                rusqlite::params![id],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO events (task_id, run_id, kind, payload_json, created_at) VALUES (NULL, NULL, 'factory_frozen', '{}', 100)",
            [],
        )
        .unwrap();
    }

    // Read-only open on v5 DB works cleanly without migrating.
    let ro_v5 = Db::open_read_only(&db_path).unwrap();
    assert_eq!(ro_v5.schema_version(), 5);
    assert_eq!(ro_v5.list_tasks(None, None).unwrap().len(), 8);
    drop(ro_v5);

    // Read-write open upgrades v5 -> v6; read-only open reads v6 cleanly.
    let rw = Db::open(&db_path).unwrap();
    assert_eq!(rw.schema_version(), 6);
    drop(rw);
    let ro_v6 = Db::open_read_only(&db_path).unwrap();
    assert_eq!(ro_v6.schema_version(), 6);
    drop(ro_v6);

    let conn = Connection::open(&db_path).unwrap();
    let has_idx: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND name='idx_events_kind_created')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(has_idx, "idx_events_kind_created must exist in v6");

    // UPDATE is still blocked.
    let upd_err = conn
        .execute("UPDATE events SET payload_json = 'x'", [])
        .unwrap_err()
        .to_string();
    assert!(upd_err.contains("append-only"), "{upd_err}");

    // DELETE blocked for task_id IS NULL and all non-terminal states.
    let null_err = conn
        .execute("DELETE FROM events WHERE task_id IS NULL", [])
        .unwrap_err()
        .to_string();
    assert!(
        null_err.contains("events is append-only for active tasks"),
        "{null_err}"
    );
    for active in [
        "t-backlog",
        "t-ready",
        "t-running",
        "t-gating",
        "t-review",
        "t-blocked",
    ] {
        let err = conn
            .execute("DELETE FROM events WHERE task_id = ?1", [active])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("events is append-only for active tasks"),
            "{active}: {err}"
        );
    }

    // DELETE allowed for done and cancelled tasks.
    assert_eq!(
        conn.execute("DELETE FROM events WHERE task_id = 't-done'", [])
            .unwrap(),
        1
    );
    assert_eq!(
        conn.execute("DELETE FROM events WHERE task_id = 't-cancelled'", [])
            .unwrap(),
        1
    );
}

#[test]
fn archive_old_terminal_events_dry_run_and_crash_safe_write_then_delete() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    fs::create_dir_all(&data).unwrap();
    let db_path = data.join("aaf.db");
    let db = Db::open(&db_path).unwrap();

    for (id, state) in [
        ("t-done", TaskState::Done),
        ("t-done-span", TaskState::Done),
        ("t-cancelled", TaskState::Cancelled),
        ("t-running", TaskState::Running),
        ("t-ready", TaskState::Ready),
    ] {
        db.insert_task(&sample_task(id, "/repo", state)).unwrap();
    }

    let now: i64 = 1_700_000_000 + 30 * 86_400;
    let old_ts = now - 20 * 86_400;
    let fresh_ts = now - 2 * 86_400;
    let emit = |kind, payload, ts, task: Option<&str>| {
        let mut ev = FactoryEvent::new(kind, payload, ts);
        if let Some(tid) = task {
            ev = ev.for_task(tid);
        }
        db.emit(&ev).unwrap()
    };

    let id_done_old = emit(
        EventKind::TaskCreated,
        r#"{"who":"done-old"}"#,
        old_ts,
        Some("t-done"),
    );
    let id_canc_old = emit(
        EventKind::RunFinished,
        r#"{"who":"canc-old"}"#,
        old_ts,
        Some("t-cancelled"),
    );
    // Task straddling the cutoff (old + fresh event) must NEVER be split by archival.
    emit(
        EventKind::GateResult,
        r#"{"who":"span-old","status":"BLOCKED"}"#,
        old_ts,
        Some("t-done-span"),
    );
    emit(
        EventKind::GateResult,
        r#"{"who":"span-fresh","status":"CLEAR"}"#,
        fresh_ts,
        Some("t-done-span"),
    );
    emit(
        EventKind::RunStarted,
        r#"{"who":"run-old"}"#,
        old_ts,
        Some("t-running"),
    );
    emit(
        EventKind::TaskCreated,
        r#"{"who":"ready-old"}"#,
        old_ts,
        Some("t-ready"),
    );
    emit(
        EventKind::FactoryFrozen,
        r#"{"who":"global-old"}"#,
        old_ts,
        None,
    );
    drop(db);

    // 1. Dry-run counts eligible rows without creating archives/ or deleting DB rows.
    let dry = archive_old_terminal_events(&data, now, 14, true).unwrap();
    assert_eq!(dry.events_archived, 2);
    assert!(dry.archive_path.is_none());
    assert!(!data.join("archives").exists());
    assert_eq!(count_events(&db_path, ""), 7);

    // 2. Real archival writes 0600 JSONL under 0700 archives/ and deletes only the 2 archived rows.
    let real = archive_old_terminal_events(&data, now, 14, false).unwrap();
    assert_eq!(real.events_archived, 2);
    let archive_path = real.archive_path.expect("archive path must be returned");
    assert!(archive_path.is_file());
    assert!(
        archive_path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .ends_with(".jsonl")
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(data.join("archives"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&archive_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    let lines: Vec<Value> = fs::read_to_string(&archive_path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    let archived_ids: Vec<i64> = lines.iter().map(|v| v["id"].as_i64().unwrap()).collect();
    assert_eq!(archived_ids, vec![id_done_old, id_canc_old]);

    for entry in fs::read_dir(data.join("archives")).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(!name.ends_with(".tmp"), "unexpected temp file {name}");
    }

    assert_eq!(count_events(&db_path, ""), 5);
    assert_eq!(
        count_events(&db_path, "WHERE task_id = 't-done-span'"),
        2,
        "straddling terminal task must retain both old and fresh events"
    );
    assert_eq!(
        count_events(
            &db_path,
            "WHERE task_id IN ('t-running', 't-ready') OR task_id IS NULL"
        ),
        3
    );

    // 3. Re-running is idempotent.
    let again = archive_old_terminal_events(&data, now, 14, false).unwrap();
    assert_eq!(again.events_archived, 0);
    assert!(again.archive_path.is_none());
}

#[test]
fn clean_expired_proofs_deletes_pairs_and_mirrors_while_protecting_receipts_symlinks_and_locks() {
    let tmp = tempfile::tempdir().unwrap();
    let (data, repo, wt) = (
        tmp.path().join("data"),
        tmp.path().join("repo"),
        tmp.path().join("wt-1"),
    );
    for d in [&data, &repo, &wt] {
        fs::create_dir_all(d).unwrap();
    }
    let twenty_days = Duration::from_secs(20 * 86_400);

    // 1. Expired pair + its mirror in common repo dir (different stems for .png vs .proof.json!).
    let expired_fp = "git-tree:expired1111";
    let (exp_wt_png, exp_wt_json) = write_proof_pair(&data, &wt, expired_fp);
    let (exp_repo_png, exp_repo_json) = write_proof_pair(&data, &repo, expired_fp);
    assert_ne!(exp_wt_png.file_stem(), exp_wt_json.file_stem());
    for p in [&exp_wt_png, &exp_wt_json, &exp_repo_png, &exp_repo_json] {
        set_mtime_past(p, twenty_days);
    }

    // 2. Expired pair protected by receipts/<repo_id>/latest.json.
    let protected_fp = "git-tree:protected2222";
    let (prot_png, prot_json) = write_proof_pair(&data, &repo, protected_fp);
    for p in [&prot_png, &prot_json] {
        set_mtime_past(p, twenty_days);
    }
    write_receipt_for(&data, &repo, protected_fp);

    // 3. Fresh pair must stay.
    let (fresh_png, fresh_json) = write_proof_pair(&data, &repo, "git-tree:fresh3333");

    // 4. Symlink file and symlink dir inside proofs/ must be skipped.
    let outside_target = tmp.path().join("outside-secret.png");
    fs::write(&outside_target, b"keep-me").unwrap();
    set_mtime_past(&outside_target, twenty_days);
    let sym_png = exp_wt_png.parent().unwrap().join("escape.png");
    let outside_dir = tmp.path().join("outside-dir");
    fs::create_dir_all(&outside_dir).unwrap();
    let outside_dir_png = outside_dir.join("stay.png");
    fs::write(&outside_dir_png, b"stay").unwrap();
    set_mtime_past(&outside_dir_png, twenty_days);
    let sym_dir = data.join("proofs").join("escape-dir");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside_target, &sym_png).unwrap();
        std::os::unix::fs::symlink(&outside_dir, &sym_dir).unwrap();
    }

    let protected_fps = collect_protected_tree_fps(&data, None);
    assert!(protected_fps.contains(protected_fp));
    let active_tasks = HashSet::new();

    // 5. older_than_days == 0 with held run lock must skip completely.
    let locks_dir = data.join("locks");
    private::create_dir(&locks_dir).unwrap();
    let lock_file = private::open_file(&locks_dir.join("busy-repo.run.lock")).unwrap();
    lock_file.try_lock().unwrap();
    let locked = clean_expired_proofs(
        &data,
        SystemTime::now(),
        0,
        &protected_fps,
        &active_tasks,
        false,
    );
    assert!(locked.skipped_locked && exp_wt_png.exists() && exp_wt_json.exists());
    lock_file.unlock().unwrap();
    drop(lock_file);

    // 6. Dry-run deletes nothing while reporting expired pairs (wt + mirror).
    let dry = clean_expired_proofs(
        &data,
        SystemTime::now(),
        14,
        &protected_fps,
        &active_tasks,
        true,
    );
    assert!(!dry.skipped_locked && dry.proofs_removed == 4);
    assert!(
        exp_wt_png.exists()
            && exp_wt_json.exists()
            && exp_repo_png.exists()
            && exp_repo_json.exists()
    );

    // 7. Real run deletes expired pairs + mirror, keeps receipt-protected, fresh, and symlink/target.
    let real = clean_expired_proofs(
        &data,
        SystemTime::now(),
        14,
        &protected_fps,
        &active_tasks,
        false,
    );
    assert_eq!(real.proofs_removed, 4);
    assert!(
        !exp_wt_png.exists()
            && !exp_wt_json.exists()
            && !exp_repo_png.exists()
            && !exp_repo_json.exists()
    );
    assert!(
        prot_png.exists()
            && prot_json.exists()
            && aaf_fp::read_proof(&data, &repo, protected_fp).is_ok()
    );
    assert!(
        fresh_png.exists()
            && fresh_json.exists()
            && outside_target.exists()
            && outside_dir_png.exists()
    );
    #[cfg(unix)]
    assert!(sym_png.symlink_metadata().is_ok() && sym_dir.symlink_metadata().is_ok());
}

#[test]
fn lightweight_sweep_cleans_audits_when_db_ok_and_skips_when_db_corrupt() {
    let tmp = tempfile::tempdir().unwrap();
    let (data, worktrees, repo) = (
        tmp.path().join("data"),
        tmp.path().join("worktrees"),
        tmp.path().join("repo"),
    );
    for d in [&data, &worktrees, &repo] {
        fs::create_dir_all(d).unwrap();
    }
    init_git_repo(&repo);
    let repo_str = repo.to_string_lossy().into_owned();

    let db_path = data.join("aaf.db");
    let db = Db::open(&db_path).unwrap();
    db.insert_task(&sample_task("t-done", &repo_str, TaskState::Done))
        .unwrap();
    db.insert_task(&sample_task("t-active", &repo_str, TaskState::Running))
        .unwrap();
    db.emit(&FactoryEvent::new(EventKind::TaskCreated, "{}", 100).for_task("t-done"))
        .unwrap();
    drop(db);

    let wt_done = worktree::worktree_path(&repo, &worktrees, "t-done").unwrap();
    let wt_active = worktree::worktree_path(&repo, &worktrees, "t-active").unwrap();
    let locked_repo_id = "locked-other-repo-id";
    let make_audit = |dir_id: &str, name: &str| {
        let dir = data.join("audits").join(dir_id);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, br#"{"audit":true}"#).unwrap();
        path
    };

    let done_audit = make_audit(&aaf_fp::repo_id(&wt_done), "done.audit.json");
    let active_audit = make_audit(&aaf_fp::repo_id(&wt_active), "active.audit.json");
    let current_repo_audit = make_audit(&aaf_fp::repo_id(&repo), "repo.audit.json");
    let locked_repo_audit = make_audit(locked_repo_id, "locked.audit.json");
    let orphan_audit = make_audit("stale-other-wt-id", "orphan.audit.json");

    let locks_dir = data.join("locks");
    private::create_dir(&locks_dir).unwrap();
    let lock_file =
        private::open_file(&locks_dir.join(format!("{locked_repo_id}.run.lock"))).unwrap();
    lock_file.try_lock().unwrap();

    // 1. Healthy DB: cleans expired unprotected/done audits, protects active/current/locked,
    //    and never mutates aaf.db.
    let far_future = 4_000_000_000i64;
    let report = lightweight_sweep(&repo, &data, &worktrees, far_future);
    assert!(!orphan_audit.exists() && !done_audit.exists());
    assert!(
        report
            .removed_files
            .contains(&orphan_audit.display().to_string())
    );
    assert!(active_audit.exists() && current_repo_audit.exists() && locked_repo_audit.exists());
    assert!(!data.join("archives").exists());
    assert_eq!(count_events(&db_path, ""), 1);

    lock_file.unlock().unwrap();
    drop(lock_file);

    // 2. Corrupt DB: MUST skip clean_expired_audits completely!
    fs::write(&db_path, b"corrupt-not-sqlite").unwrap();
    let corrupt_target = make_audit("unprotected-when-db-corrupt", "must-stay.audit.json");
    let report_corrupt = lightweight_sweep(&repo, &data, &worktrees, far_future);
    assert!(
        report_corrupt
            .warnings
            .iter()
            .any(|w| w.contains("db state:"))
    );
    assert!(
        corrupt_target.exists(),
        "MUST NOT delete audits when collect_db_state fails"
    );
}
