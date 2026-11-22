//! `RunHistory` on the `runs` table: which account a repository or a task
//! last ran on. Derived, never stored, so it cannot drift from the runs.

use aaf_domain::account::ProviderKind;
use aaf_domain::ports::PortResult;
use rusqlite::{OptionalExtension, params};

use super::{Db, sql_err};
use crate::accounts::RunHistory;
use crate::cross_audit::AUDIT_EXECUTOR;

/// Newest first; `rowid` orders runs that started in the same second. The
/// join on `accounts` keeps only accounts of the asked provider (a run of a
/// custom executor has no account at all).
const LAST_OF_REPO: &str = "SELECT r.account_id FROM tasks t \
     JOIN runs r ON r.task_id = t.id \
     JOIN accounts a ON a.id = r.account_id \
     WHERE t.repo_root = ?1 AND a.provider = ?2 AND r.executor != ?3 \
     ORDER BY r.started_at DESC, r.rowid DESC LIMIT 1";

const LAST_OF_TASK: &str = "SELECT r.account_id FROM runs r \
     JOIN accounts a ON a.id = r.account_id \
     WHERE r.task_id = ?1 AND a.provider = ?2 AND r.executor != ?3 \
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
                    stmt.query_row(params![key, provider.as_str(), AUDIT_EXECUTOR], |row| {
                        row.get(0)
                    })
                    .optional()
                })
                .map_err(sql_err)
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
