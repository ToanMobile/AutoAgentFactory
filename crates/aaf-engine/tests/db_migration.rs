#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Schema migrations: upgrade from an old database, idempotent reopen,
//! refusal of an unknown newer schema.

use std::path::Path;

use aaf_domain::account::ProviderKind;
use aaf_domain::event::EventKind;
use aaf_domain::ports::{
    AccountStore, BudgetStore, EventSink, FingerprintStore, KvStore, RunStore, TaskStore,
};
use aaf_domain::task::{Priority, TaskState};
use aaf_engine::db::{Db, DbError, SCHEMA_VERSION};
use rusqlite::Connection;

/// The version-1 schema exactly as it shipped. Frozen here on purpose: this
/// is what old databases on disk look like, whatever the current code says.
const V1_SCHEMA: &str = "
    CREATE TABLE tasks (
        id TEXT PRIMARY KEY, repo_root TEXT NOT NULL, parent_id TEXT REFERENCES tasks(id),
        root_id TEXT NOT NULL, depth INTEGER NOT NULL, title TEXT NOT NULL, spec TEXT NOT NULL,
        context_notes TEXT NOT NULL, state TEXT NOT NULL, priority TEXT NOT NULL,
        fingerprint TEXT, blocked_reason TEXT, unblock_cycles INTEGER NOT NULL DEFAULT 0,
        created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
    CREATE TABLE root_budgets (
        root_id TEXT PRIMARY KEY, tokens_used INTEGER NOT NULL, token_limit INTEGER NOT NULL,
        wall_secs_used INTEGER NOT NULL, wall_secs_limit INTEGER NOT NULL,
        cycles_used INTEGER NOT NULL, cycles_limit INTEGER NOT NULL, frozen INTEGER NOT NULL);
    CREATE TABLE task_deps (
        task_id TEXT NOT NULL REFERENCES tasks(id), blocked_by TEXT NOT NULL REFERENCES tasks(id),
        PRIMARY KEY (task_id, blocked_by));
    CREATE TABLE runs (
        id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES tasks(id), account_id TEXT,
        executor TEXT NOT NULL, worktree_path TEXT NOT NULL, branch TEXT NOT NULL,
        status TEXT NOT NULL, exit_code INTEGER, tokens_in INTEGER NOT NULL,
        tokens_out INTEGER NOT NULL, started_at INTEGER NOT NULL, ended_at INTEGER);
    CREATE TABLE accounts (
        id TEXT PRIMARY KEY, provider TEXT NOT NULL, config_dir TEXT NOT NULL,
        cooldown_until INTEGER);
    CREATE TABLE events (
        id INTEGER PRIMARY KEY AUTOINCREMENT, task_id TEXT, run_id TEXT, kind TEXT NOT NULL,
        payload_json TEXT NOT NULL, created_at INTEGER NOT NULL);
    CREATE TABLE closed_fingerprints (
        id INTEGER PRIMARY KEY, fingerprint TEXT NOT NULL, repo_root TEXT NOT NULL,
        task_id TEXT NOT NULL, closed_at INTEGER NOT NULL);
    CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
    PRAGMA user_version = 1;
";

const V1_ROWS: &str = r#"
    INSERT INTO tasks VALUES ('root', '/repo', NULL, 'root', 0, 'old task',
        '{"description":"from v1","acceptance":["a1"]}', '["note"]', 'blocked', 'P0', 'fp-old',
        '{"kind":"needs_human","why":"legacy"}', 2, 10, 20);
    INSERT INTO tasks VALUES ('dep', '/repo', NULL, 'dep', 0, 'dep task',
        '{"description":"d","acceptance":[]}', '[]', 'ready', 'P2', NULL, NULL, 0, 11, 11);
    INSERT INTO task_deps VALUES ('root', 'dep');
    INSERT INTO root_budgets VALUES ('root', 5, 100, 6, 200, 1, 3, 1);
    INSERT INTO runs VALUES ('run-1', 'root', 'acc', 'codex', '/wt', 'br', 'succeeded', 0,
        40, 2, 15, 18);
    INSERT INTO accounts VALUES ('acc', 'codex', '/cfg', 777);
    INSERT INTO events (task_id, run_id, kind, payload_json, created_at)
        VALUES ('root', 'run-1', 'run_finished', '{"ok":true}', 18);
    INSERT INTO closed_fingerprints (fingerprint, repo_root, task_id, closed_at)
        VALUES ('fp-closed', '/repo', 'gone', 9);
    INSERT INTO meta VALUES ('factory_frozen', '1');
"#;

/// What version 2 added on top of version 1, frozen like [`V1_SCHEMA`].
const V2_STEP: &str = "
    CREATE INDEX idx_tasks_state_priority ON tasks(state, priority, created_at);
    CREATE INDEX idx_tasks_repo_state_priority ON tasks(repo_root, state, priority, created_at);
    CREATE INDEX idx_tasks_fingerprint ON tasks(fingerprint, repo_root);
    CREATE INDEX idx_tasks_parent ON tasks(parent_id);
    CREATE INDEX idx_runs_account_started ON runs(account_id, started_at);
    CREATE INDEX idx_runs_task ON runs(task_id, started_at);
    CREATE INDEX idx_events_task ON events(task_id, id);
    CREATE INDEX idx_closed_fingerprints ON closed_fingerprints(fingerprint, repo_root, closed_at);
    CREATE TRIGGER events_no_update BEFORE UPDATE ON events
        BEGIN SELECT RAISE(ABORT, 'events is append-only'); END;
    CREATE TRIGGER events_no_delete BEFORE DELETE ON events
        BEGIN SELECT RAISE(ABORT, 'events is append-only'); END;
    PRAGMA user_version = 2;
";

fn account_columns(path: &Path) -> Vec<String> {
    let conn = Connection::open(path).unwrap();
    let mut stmt = conn
        .prepare("SELECT name FROM pragma_table_info('accounts')")
        .unwrap();
    stmt.query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn version_2_database_gains_the_account_name_column_and_keeps_its_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aaf.db");
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(V1_SCHEMA).unwrap();
        conn.execute_batch(V1_ROWS).unwrap();
        conn.execute_batch(V2_STEP).unwrap();
        conn.execute_batch(
            "INSERT INTO accounts VALUES ('claude_code-abc', 'claude_code', '/h/.claude-work', NULL)",
        )
        .unwrap();
    }
    assert_eq!(user_version(&path), 2);
    assert!(!account_columns(&path).contains(&"name".to_string()));

    let db = Db::open(&path).unwrap();
    let accounts = db.list_accounts().unwrap();
    assert_eq!(accounts.len(), 2);
    // Rows written before the column existed get the default display name.
    assert_eq!(accounts[0].profile.id, "acc");
    assert_eq!(accounts[0].profile.config_dir, "/cfg");
    assert_eq!(accounts[0].profile.name, "cfg");
    assert_eq!(accounts[0].cooldown_until, Some(777));
    assert_eq!(accounts[1].profile.id, "claude_code-abc");
    assert_eq!(accounts[1].profile.name, "work");

    // Rediscovery stores the real name and keeps the cooldown.
    let mut renamed = accounts[0].profile.clone();
    renamed.name = "Main".to_string();
    db.upsert_account(&renamed).unwrap();
    let again = db.list_accounts().unwrap();
    assert_eq!(again[0].profile.name, "Main");
    assert_eq!(again[0].cooldown_until, Some(777));
    assert_eq!(db.list_tasks(None, None).unwrap().len(), 2);
    drop(db);

    assert_eq!(user_version(&path), SCHEMA_VERSION);
    const { assert!(SCHEMA_VERSION >= 3) };
    assert_eq!(
        account_columns(&path),
        ["id", "provider", "config_dir", "cooldown_until", "name"]
    );
    // Reopening the upgraded file is a no-op.
    let dump = schema_dump(&path);
    drop(Db::open(&path).unwrap());
    assert_eq!(schema_dump(&path), dump);
}

fn user_version(path: &Path) -> i64 {
    let conn = Connection::open(path).unwrap();
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap()
}

/// Every schema object with its SQL, in a stable order.
fn schema_dump(path: &Path) -> Vec<(String, String, Option<String>)> {
    let conn = Connection::open(path).unwrap();
    let mut stmt = conn
        .prepare("SELECT type, name, sql FROM sqlite_master ORDER BY type, name")
        .unwrap();
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn names_of(dump: &[(String, String, Option<String>)], kind: &str) -> Vec<String> {
    dump.iter()
        .filter(|(ty, name, _)| ty == kind && !name.starts_with("sqlite_"))
        .map(|(_, name, _)| name.clone())
        .collect()
}

#[test]
fn version_1_database_is_upgraded_and_its_rows_survive() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aaf.db");
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(V1_SCHEMA).unwrap();
        conn.execute_batch(V1_ROWS).unwrap();
    }
    assert_eq!(user_version(&path), 1);
    assert!(names_of(&schema_dump(&path), "index").is_empty());

    let db = Db::open(&path).unwrap();

    let root = db.get_task("root").unwrap();
    assert_eq!(root.title, "old task");
    assert_eq!(root.spec.description, "from v1");
    assert_eq!(root.spec.acceptance, ["a1"]);
    assert_eq!(root.context_notes, ["note"]);
    assert_eq!(root.state, TaskState::Blocked);
    assert_eq!(root.priority, Priority::P0);
    assert_eq!(root.blocked_reason.unwrap().kind(), "needs_human");
    assert_eq!(
        (root.unblock_cycles, root.created_at, root.updated_at),
        (2, 10, 20)
    );
    assert_eq!(db.list_tasks(None, None).unwrap().len(), 2);
    assert_eq!(db.deps_of("root").unwrap(), ["dep"]);

    let budget = db.get_budget("root").unwrap();
    assert_eq!((budget.tokens_used, budget.token_limit), (5, 100));
    assert_eq!((budget.cycles_used, budget.frozen), (1, true));

    let runs = db.runs_for_task("root").unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!((runs[0].tokens_in, runs[0].ended_at), (40, Some(18)));
    assert_eq!(db.tokens_since("acc", 15).unwrap(), 42);

    let accounts = db.list_accounts().unwrap();
    assert_eq!(accounts[0].profile.provider, ProviderKind::Codex);
    assert_eq!(accounts[0].cooldown_until, Some(777));

    let events = db.events_for_task("root", 10).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, EventKind::RunFinished);
    assert_eq!(events[0].payload_json, r#"{"ok":true}"#);

    let closed = db.find_closed_since("fp-closed", "/repo", 0).unwrap();
    assert_eq!(closed.unwrap().task_id, "gone");
    assert_eq!(
        db.find_open_task("fp-old", "/repo").unwrap(),
        Some("root".to_string())
    );
    assert_eq!(
        db.get_meta("factory_frozen").unwrap(),
        Some("1".to_string())
    );

    // The upgraded database accepts new writes.
    db.set_state("root", TaskState::Ready, None, 30).unwrap();
    drop(db);

    assert_eq!(user_version(&path), SCHEMA_VERSION);
    // Compile-time: with a single version this upgrade test would be a no-op.
    const { assert!(SCHEMA_VERSION > 1) };
    let dump = schema_dump(&path);
    let indexes = names_of(&dump, "index");
    for expected in [
        "idx_tasks_state_priority",
        "idx_tasks_fingerprint",
        "idx_runs_account_started",
        "idx_events_task",
    ] {
        assert!(indexes.iter().any(|name| name == expected), "{expected}");
    }
    assert_eq!(
        names_of(&dump, "trigger"),
        ["events_no_delete", "events_no_update"]
    );
}

#[test]
fn opening_twice_changes_neither_schema_nor_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aaf.db");

    let db = Db::open(&path).unwrap();
    db.set_meta("k", "v").unwrap();
    drop(db);
    let version = user_version(&path);
    let dump = schema_dump(&path);
    assert_eq!(version, SCHEMA_VERSION);
    assert_eq!(
        names_of(&dump, "table"),
        [
            "accounts",
            "closed_fingerprints",
            "events",
            "meta",
            "root_budgets",
            "runs",
            "task_deps",
            "tasks"
        ]
    );

    let db = Db::open(&path).unwrap();
    assert_eq!(db.get_meta("k").unwrap(), Some("v".to_string()));
    drop(db);
    assert_eq!(user_version(&path), version);
    assert_eq!(schema_dump(&path), dump);
}

#[test]
fn fresh_database_is_in_wal_mode() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aaf.db");
    let db = Db::open(&path).unwrap();
    // journal_mode is a property of the file, so another connection sees it.
    let conn = Connection::open(&path).unwrap();
    let mode: String = conn
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    drop(db);
}

#[test]
fn database_from_a_newer_build_is_refused_and_left_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aaf.db");
    let future = SCHEMA_VERSION + 1;
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE from_the_future (x)")
            .unwrap();
        conn.pragma_update(None, "user_version", future).unwrap();
    }

    let err = Db::open(&path).unwrap_err();
    assert!(
        matches!(err, DbError::UnsupportedSchema { found, supported }
            if found == future && supported == SCHEMA_VERSION),
        "{err:?}"
    );
    assert_eq!(user_version(&path), future);
    assert_eq!(names_of(&schema_dump(&path), "table"), ["from_the_future"]);
}

#[test]
fn opening_a_path_that_cannot_be_created_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing-dir").join("aaf.db");
    assert!(matches!(Db::open(&path), Err(DbError::Sqlite(_))));
}
