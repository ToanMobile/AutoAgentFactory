//! PR-feedback-specific read-only SQLite queries (run-window overlap check).

use aaf_domain::ports::PortResult;
use rusqlite::params;

use super::{Db, sql_err};

impl Db {
    /// Returns `true` when any run for a task in `repo_root` overlaps timestamp
    /// `at` (`started_at <= at <= COALESCE(ended_at, now)`).
    pub fn has_overlapping_run(&self, repo_root: &str, at: i64, now: i64) -> PortResult<bool> {
        let repo_root = repo_root.to_string();
        self.read(move |conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT EXISTS( \
                         SELECT 1 FROM runs r \
                         JOIN tasks t ON t.id = r.task_id \
                         WHERE t.repo_root = ?1 \
                           AND r.started_at <= ?2 \
                           AND COALESCE(r.ended_at, ?3) >= ?2 \
                     )",
                )
                .map_err(sql_err)?;
            let found: i64 = stmt
                .query_row(params![repo_root, at, now], |row| row.get(0))
                .map_err(sql_err)?;
            Ok(found != 0)
        })
    }
}
