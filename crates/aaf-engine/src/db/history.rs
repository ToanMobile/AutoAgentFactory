//! `RunHistory` on the `runs` table and retention/archival queries on `events`.

use std::collections::HashSet;

use aaf_domain::account::ProviderKind;
use aaf_domain::engine_contracts::{AUDIT_EXECUTOR, PLAN_EXECUTOR, PROPOSER_EXECUTOR, RunHistory};
use aaf_domain::ports::PortResult;
use rusqlite::{OptionalExtension, TransactionBehavior, params, params_from_iter};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Db, sql_err};

const ARCHIVE_DELETE_CHUNK_SIZE: usize = 500;

/// Raw `events` row serialized to JSONL during terminal-event archival.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchivedEventRow {
    pub id: i64,
    pub task_id: Option<String>,
    pub run_id: Option<String>,
    pub kind: String,
    pub payload_json: String,
    pub created_at: i64,
}

/// Newest first; `rowid` orders runs that started in the same second. The
/// join on `accounts` keeps only accounts of the asked provider (a run of a
/// custom executor has no account at all).
const LAST_OF_REPO: &str = "SELECT r.account_id FROM tasks t \
     JOIN runs r ON r.task_id = t.id \
     JOIN accounts a ON a.id = r.account_id \
     WHERE t.repo_root = ?1 AND a.provider = ?2 AND r.executor NOT IN (?3, ?4, ?5) \
     ORDER BY r.started_at DESC, r.rowid DESC LIMIT 1";

const LAST_OF_TASK: &str = "SELECT r.account_id FROM runs r \
     JOIN accounts a ON a.id = r.account_id \
     WHERE r.task_id = ?1 AND a.provider = ?2 AND r.executor NOT IN (?3, ?4, ?5) \
     ORDER BY r.started_at DESC, r.rowid DESC LIMIT 1";

impl Db {
    fn last_account(
        &self,
        sql: &'static str,
        key: &str,
        provider: ProviderKind,
    ) -> PortResult<Option<String>> {
        let key = key.to_string();
        self.read(move |conn| {
            conn.prepare_cached(sql)
                .and_then(|mut stmt| {
                    stmt.query_row(
                        params![
                            key,
                            provider.as_str(),
                            AUDIT_EXECUTOR,
                            PLAN_EXECUTOR,
                            PROPOSER_EXECUTOR
                        ],
                        |row| row.get(0),
                    )
                    .optional()
                })
                .map_err(sql_err)
        })
    }

    /// Lists `events` rows belonging to terminal tasks (`done` or `cancelled`)
    /// whose **entire** event history is older than `cutoff_epoch`, ordered by
    /// `id ASC`. Tasks straddling `cutoff_epoch` are never split. Safe on
    /// read-only handles.
    pub fn list_archivable_terminal_events(
        &self,
        cutoff_epoch: i64,
    ) -> PortResult<Vec<ArchivedEventRow>> {
        self.read(move |conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT e.id, e.task_id, e.run_id, e.kind, e.payload_json, e.created_at \
                     FROM events e \
                     JOIN tasks t ON t.id = e.task_id \
                     WHERE t.state IN ('done', 'cancelled') \
                       AND e.created_at < ?1 \
                       AND NOT EXISTS ( \
                           SELECT 1 FROM events e2 \
                           WHERE e2.task_id = e.task_id AND e2.created_at >= ?1 \
                       ) \
                     ORDER BY e.id ASC",
                )
                .map_err(sql_err)?;
            let mut rows = stmt.query(params![cutoff_epoch]).map_err(sql_err)?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().map_err(sql_err)? {
                out.push(ArchivedEventRow {
                    id: row.get(0).map_err(sql_err)?,
                    task_id: row.get(1).map_err(sql_err)?,
                    run_id: row.get(2).map_err(sql_err)?,
                    kind: row.get(3).map_err(sql_err)?,
                    payload_json: row.get(4).map_err(sql_err)?,
                    created_at: row.get(5).map_err(sql_err)?,
                });
            }
            Ok(out)
        })
    }

    /// Deletes exact archived `events.id` rows in one `BEGIN IMMEDIATE`
    /// transaction and runs a passive WAL checkpoint (never full `VACUUM`).
    pub fn delete_archived_events(&self, ids: &[i64]) -> PortResult<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let ids = ids.to_vec();
        self.call(move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_err)?;
            let mut deleted = 0usize;
            for chunk in ids.chunks(ARCHIVE_DELETE_CHUNK_SIZE) {
                let ph = (1..=chunk.len())
                    .map(|i| format!("?{i}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let sql = format!("DELETE FROM events WHERE id IN ({ph})");
                let mut stmt = tx.prepare(&sql).map_err(sql_err)?;
                deleted += stmt
                    .execute(params_from_iter(chunk.iter()))
                    .map_err(sql_err)?;
            }
            tx.commit().map_err(sql_err)?;
            let _ = conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()));
            Ok(deleted)
        })
    }

    /// Extracts non-empty `tree_fp` strings recorded in events of non-terminal
    /// tasks so proof retention never deletes active task proofs.
    pub fn active_task_tree_fps(&self) -> PortResult<HashSet<String>> {
        self.read(move |conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT e.payload_json \
                     FROM events e \
                     JOIN tasks t ON t.id = e.task_id \
                     WHERE t.state NOT IN ('done', 'cancelled') \
                       AND e.kind IN ('gate_result', 'cross_audit', 'proof_recorded')",
                )
                .map_err(sql_err)?;
            let mut rows = stmt.query([]).map_err(sql_err)?;
            let mut fps = HashSet::new();
            while let Some(row) = rows.next().map_err(sql_err)? {
                let payload: String = row.get(0).map_err(sql_err)?;
                if let Ok(val) = serde_json::from_str::<Value>(&payload)
                    && let Some(fp) = val.get("tree_fp").and_then(Value::as_str)
                {
                    let trimmed = fp.trim();
                    if !trimmed.is_empty() {
                        fps.insert(trimmed.to_string());
                    }
                }
            }
            Ok(fps)
        })
    }
}

impl RunHistory for Db {
    fn last_account_of_repo(
        &self,
        repo_root: &str,
        provider: ProviderKind,
    ) -> PortResult<Option<String>> {
        self.last_account(LAST_OF_REPO, repo_root, provider)
    }

    fn last_account_of_task(
        &self,
        task_id: &str,
        provider: ProviderKind,
    ) -> PortResult<Option<String>> {
        self.last_account(LAST_OF_TASK, task_id, provider)
    }
}
