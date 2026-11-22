//! `turn_snapshots` table operations (schema v4): pre-turn git snapshot refs
//! recorded before each implementation turn so a human can roll back cleanly.

use aaf_domain::ports::PortResult;
use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use super::{Db, sql_err};

/// One pre-turn snapshot recorded for a task before an implementation turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnSnapshot {
    pub id: i64,
    pub task_id: String,
    pub run_id: String,
    pub ref_name: String,
    pub commit_oid: String,
    pub created_at: i64,
}

fn snapshot_from_row(row: &Row<'_>) -> rusqlite::Result<TurnSnapshot> {
    Ok(TurnSnapshot {
        id: row.get(0)?,
        task_id: row.get(1)?,
        run_id: row.get(2)?,
        ref_name: row.get(3)?,
        commit_oid: row.get(4)?,
        created_at: row.get(5)?,
    })
}

impl Db {
    /// Inserts one pre-turn snapshot row and returns its autoincremented id.
    pub fn insert_snapshot(
        &self,
        task_id: &str,
        run_id: &str,
        ref_name: &str,
        commit_oid: &str,
        created_at: i64,
    ) -> PortResult<i64> {
        let task_id = task_id.to_string();
        let run_id = run_id.to_string();
        let ref_name = ref_name.to_string();
        let commit_oid = commit_oid.to_string();
        self.write(move |tx| {
            tx.prepare_cached(
                "INSERT INTO turn_snapshots (task_id, run_id, ref_name, commit_oid, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .and_then(|mut stmt| {
                stmt.insert(params![task_id, run_id, ref_name, commit_oid, created_at])
            })
            .map_err(sql_err)
        })
    }

    /// Returns the most recent snapshot for `task_id`, or `Ok(None)` when none
    /// exists or the database was opened on a schema older than v4.
    pub fn latest_snapshot(&self, task_id: &str) -> PortResult<Option<TurnSnapshot>> {
        if self.schema_version() < 4 {
            return Ok(None);
        }
        let task_id = task_id.to_string();
        self.read(move |conn| {
            conn.prepare_cached(
                "SELECT id, task_id, run_id, ref_name, commit_oid, created_at \
                 FROM turn_snapshots WHERE task_id = ?1 ORDER BY id DESC LIMIT 1",
            )
            .and_then(|mut stmt| {
                stmt.query_row(params![task_id], snapshot_from_row)
                    .optional()
            })
            .map_err(sql_err)
        })
    }

    /// Deletes one snapshot row by primary key; returns `true` when a row was removed.
    pub fn delete_snapshot(&self, id: i64) -> PortResult<bool> {
        if self.schema_version() < 4 {
            return Ok(false);
        }
        self.write(move |tx| {
            let changed = tx
                .prepare_cached("DELETE FROM turn_snapshots WHERE id = ?1")
                .and_then(|mut stmt| stmt.execute(params![id]))
                .map_err(sql_err)?;
            Ok(changed > 0)
        })
    }

    /// Lists all snapshots recorded for `task_id`, oldest first.
    pub fn list_snapshots(&self, task_id: &str) -> PortResult<Vec<TurnSnapshot>> {
        if self.schema_version() < 4 {
            return Ok(Vec::new());
        }
        let task_id = task_id.to_string();
        self.read(move |conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT id, task_id, run_id, ref_name, commit_oid, created_at \
                     FROM turn_snapshots WHERE task_id = ?1 ORDER BY id ASC",
                )
                .map_err(sql_err)?;
            let mut rows = stmt.query(params![task_id]).map_err(sql_err)?;
            let mut snaps = Vec::new();
            while let Some(row) = rows.next().map_err(sql_err)? {
                snaps.push(snapshot_from_row(row).map_err(sql_err)?);
            }
            Ok(snaps)
        })
    }
}
