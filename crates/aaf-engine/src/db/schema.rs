//! Versioned, forward-only migrations tracked in `PRAGMA user_version`.

use rusqlite::{Connection, TransactionBehavior};

use super::DbError;

/// Step `i` (0-based) upgrades a database from version `i` to `i + 1`.
/// Released steps are never edited; schema changes append a new step. A step
/// runs exactly once: `migrate` applies it and bumps the version in one
/// immediate transaction (`ADD COLUMN` has no `IF NOT EXISTS` form).
const MIGRATIONS: &[&str] = &[
    // v1: tables.
    "
    CREATE TABLE IF NOT EXISTS tasks (
        id             TEXT PRIMARY KEY,
        repo_root      TEXT NOT NULL,
        parent_id      TEXT REFERENCES tasks(id),
        root_id        TEXT NOT NULL,
        depth          INTEGER NOT NULL,
        title          TEXT NOT NULL,
        spec           TEXT NOT NULL,
        context_notes  TEXT NOT NULL,
        state          TEXT NOT NULL,
        priority       TEXT NOT NULL,
        fingerprint    TEXT,
        blocked_reason TEXT,
        unblock_cycles INTEGER NOT NULL DEFAULT 0,
        created_at     INTEGER NOT NULL,
        updated_at     INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS root_budgets (
        root_id         TEXT PRIMARY KEY,
        tokens_used     INTEGER NOT NULL,
        token_limit     INTEGER NOT NULL,
        wall_secs_used  INTEGER NOT NULL,
        wall_secs_limit INTEGER NOT NULL,
        cycles_used     INTEGER NOT NULL,
        cycles_limit    INTEGER NOT NULL,
        frozen          INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS task_deps (
        task_id    TEXT NOT NULL REFERENCES tasks(id),
        blocked_by TEXT NOT NULL REFERENCES tasks(id),
        PRIMARY KEY (task_id, blocked_by)
    );
    CREATE TABLE IF NOT EXISTS runs (
        id            TEXT PRIMARY KEY,
        task_id       TEXT NOT NULL REFERENCES tasks(id),
        account_id    TEXT,
        executor      TEXT NOT NULL,
        worktree_path TEXT NOT NULL,
        branch        TEXT NOT NULL,
        status        TEXT NOT NULL,
        exit_code     INTEGER,
        tokens_in     INTEGER NOT NULL,
        tokens_out    INTEGER NOT NULL,
        started_at    INTEGER NOT NULL,
        ended_at      INTEGER
    );
    CREATE TABLE IF NOT EXISTS accounts (
        id             TEXT PRIMARY KEY,
        provider       TEXT NOT NULL,
        config_dir     TEXT NOT NULL,
        cooldown_until INTEGER
    );
    CREATE TABLE IF NOT EXISTS events (
        id           INTEGER PRIMARY KEY AUTOINCREMENT,
        task_id      TEXT,
        run_id       TEXT,
        kind         TEXT NOT NULL,
        payload_json TEXT NOT NULL,
        created_at   INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS closed_fingerprints (
        id          INTEGER PRIMARY KEY,
        fingerprint TEXT NOT NULL,
        repo_root   TEXT NOT NULL,
        task_id     TEXT NOT NULL,
        closed_at   INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS meta (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    ",
    // v2: indexes for the port queries, and the append-only guard on events.
    "
    CREATE INDEX IF NOT EXISTS idx_tasks_state_priority
        ON tasks(state, priority, created_at);
    CREATE INDEX IF NOT EXISTS idx_tasks_repo_state_priority
        ON tasks(repo_root, state, priority, created_at);
    CREATE INDEX IF NOT EXISTS idx_tasks_fingerprint
        ON tasks(fingerprint, repo_root);
    CREATE INDEX IF NOT EXISTS idx_tasks_parent
        ON tasks(parent_id);
    CREATE INDEX IF NOT EXISTS idx_runs_account_started
        ON runs(account_id, started_at);
    CREATE INDEX IF NOT EXISTS idx_runs_task
        ON runs(task_id, started_at);
    CREATE INDEX IF NOT EXISTS idx_events_task
        ON events(task_id, id);
    CREATE INDEX IF NOT EXISTS idx_closed_fingerprints
        ON closed_fingerprints(fingerprint, repo_root, closed_at);
    CREATE TRIGGER IF NOT EXISTS events_no_update BEFORE UPDATE ON events
        BEGIN SELECT RAISE(ABORT, 'events is append-only'); END;
    CREATE TRIGGER IF NOT EXISTS events_no_delete BEFORE DELETE ON events
        BEGIN SELECT RAISE(ABORT, 'events is append-only'); END;
    ",
    // v3: display name of an account; empty until the account is rediscovered.
    "
    ALTER TABLE accounts ADD COLUMN name TEXT NOT NULL DEFAULT '';
    ",
    // v4: task scope_paths for scope-keyed parallelism and turn_snapshots for rollback.
    "
    ALTER TABLE tasks ADD COLUMN scope_paths TEXT NOT NULL DEFAULT '[]';
    CREATE TABLE IF NOT EXISTS turn_snapshots (
        id         INTEGER PRIMARY KEY AUTOINCREMENT,
        task_id    TEXT NOT NULL REFERENCES tasks(id),
        run_id     TEXT NOT NULL,
        ref_name   TEXT NOT NULL,
        commit_oid TEXT NOT NULL,
        created_at INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_turn_snapshots_task
        ON turn_snapshots(task_id, id);
    ",
    // v5: persisted transient retry scheduling and auto-recovery counters.
    "
    ALTER TABLE tasks ADD COLUMN retry_after INTEGER;
    ALTER TABLE tasks ADD COLUMN auto_recover_count INTEGER NOT NULL DEFAULT 0;
    CREATE INDEX IF NOT EXISTS idx_tasks_blocked_retry
        ON tasks(repo_root, state, retry_after)
        WHERE state = 'blocked' AND retry_after IS NOT NULL;
    ",
];

/// Schema version this build reads and writes.
pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

fn user_version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
}

pub(super) fn migrate(conn: &mut Connection) -> Result<(), DbError> {
    // The version is read under the write lock, so two processes opening the
    // same fresh file cannot both apply a step.
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let found = user_version(&tx)?;
    if !(0..=SCHEMA_VERSION).contains(&found) {
        // Fail closed: an older build must not write into a schema it does
        // not know.
        return Err(DbError::UnsupportedSchema {
            found,
            supported: SCHEMA_VERSION,
        });
    }
    let mut version = found;
    for step in MIGRATIONS.iter().skip(found as usize) {
        tx.execute_batch(step)?;
        version += 1;
        tx.pragma_update(None, "user_version", version)?;
    }
    tx.commit()?;
    Ok(())
}
