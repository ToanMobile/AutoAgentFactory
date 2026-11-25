//! Machine-wide rolling token usage queries for the 24h cap and 1h spike guard.

use aaf_domain::ports::PortResult;
use rusqlite::params;

use super::{Db, sql_err, to_u64};

impl Db {
    /// Returns `(total_tokens, earliest_nonzero_ts)` across every run in the
    /// database with `COALESCE(ended_at, started_at) >= since`, regardless of
    /// `account_id` or repo.
    pub fn cost_window_usage(&self, since: i64) -> PortResult<(u64, Option<i64>)> {
        self.read(move |conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT COALESCE(SUM(tokens_in + tokens_out), 0), \
                     MIN(CASE WHEN tokens_in + tokens_out > 0 THEN COALESCE(ended_at, started_at) END) \
                     FROM runs WHERE COALESCE(ended_at, started_at) >= ?1",
                )
                .map_err(sql_err)?;
            let (sum_raw, earliest): (i64, Option<i64>) = stmt
                .query_row(params![since], |row| Ok((row.get(0)?, row.get(1)?)))
                .map_err(sql_err)?;
            let total = to_u64(sum_raw, "sum of runs tokens")?;
            Ok((total, earliest))
        })
    }

    /// Aggregates `(account_id, tokens_in, tokens_out)` grouped by `runs.account_id`,
    /// optionally scoped to tasks under `repo_root`.
    pub fn run_tokens_by_account(
        &self,
        repo_root: Option<&str>,
    ) -> PortResult<Vec<(Option<String>, u64, u64)>> {
        let repo_root = repo_root.map(str::to_string);
        self.read(move |conn| {
            let map_row = |row: &rusqlite::Row<'_>| -> PortResult<(Option<String>, u64, u64)> {
                let account_id: Option<String> = row.get(0).map_err(sql_err)?;
                let tokens_in = to_u64(row.get(1).map_err(sql_err)?, "runs.tokens_in")?;
                let tokens_out = to_u64(row.get(2).map_err(sql_err)?, "runs.tokens_out")?;
                Ok((account_id, tokens_in, tokens_out))
            };
            let mut totals = Vec::new();
            if let Some(root) = repo_root {
                let mut stmt = conn
                    .prepare_cached(
                        "SELECT r.account_id, COALESCE(SUM(r.tokens_in), 0), \
                         COALESCE(SUM(r.tokens_out), 0) \
                         FROM runs r INNER JOIN tasks t ON t.id = r.task_id \
                         WHERE t.repo_root = ?1 GROUP BY r.account_id",
                    )
                    .map_err(sql_err)?;
                let mut rows = stmt.query(params![root]).map_err(sql_err)?;
                while let Some(row) = rows.next().map_err(sql_err)? {
                    totals.push(map_row(row)?);
                }
            } else {
                let mut stmt = conn
                    .prepare_cached(
                        "SELECT account_id, COALESCE(SUM(tokens_in), 0), \
                         COALESCE(SUM(tokens_out), 0) \
                         FROM runs GROUP BY account_id",
                    )
                    .map_err(sql_err)?;
                let mut rows = stmt.query([]).map_err(sql_err)?;
                while let Some(row) = rows.next().map_err(sql_err)? {
                    totals.push(map_row(row)?);
                }
            }
            Ok(totals)
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use aaf_domain::ports::{RunStore, TaskStore};
    use aaf_domain::run::{RunRecord, RunStatus};
    use aaf_domain::task::{ImmutableSpec, Priority, Task};

    fn make_task(id: &str, repo: &str) -> Task {
        Task::new_root(
            id,
            repo,
            format!("title {id}"),
            ImmutableSpec {
                description: "desc".into(),
                acceptance: vec!["ok".into()],
            },
            Priority::P2,
            100,
        )
    }

    fn make_run(
        id: &str,
        task_id: &str,
        account_id: Option<&str>,
        started_at: i64,
        ended_at: Option<i64>,
        tokens: (u64, u64),
    ) -> RunRecord {
        RunRecord {
            id: id.into(),
            task_id: task_id.into(),
            account_id: account_id.map(str::to_string),
            executor: "claude".into(),
            worktree_path: String::new(),
            branch: "b".into(),
            status: RunStatus::Succeeded,
            exit_code: Some(0),
            tokens_in: tokens.0,
            tokens_out: tokens.1,
            started_at,
            ended_at,
        }
    }

    #[test]
    fn cost_window_usage_uses_ended_at_fallback_started_at_and_includes_null_account() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
        assert_eq!(db.cost_window_usage(0).unwrap(), (0, None));

        db.insert_task(&make_task("t1", "/repo-a")).unwrap();
        db.insert_task(&make_task("t2", "/repo-b")).unwrap();

        // Ended before window -> ignored.
        db.insert_run(&make_run(
            "r0",
            "t1",
            Some("acc1"),
            900,
            Some(999),
            (500, 500),
        ))
        .unwrap();
        // Zero-token run inside window -> must NOT anchor earliest_nonzero.
        db.insert_run(&make_run("r1", "t1", None, 1_000, Some(1_050), (0, 0)))
            .unwrap();
        // Long run started BEFORE window, ended INSIDE window, NULL account -> MUST count.
        db.insert_run(&make_run("r2", "t1", None, 500, Some(1_200), (300, 100)))
            .unwrap();
        // Unended run falls back to started_at -> MUST count.
        db.insert_run(&make_run("r3", "t2", Some("acc2"), 1_500, None, (200, 50)))
            .unwrap();

        assert_eq!(db.cost_window_usage(1_000).unwrap(), (650, Some(1_200)));
        assert_eq!(db.cost_window_usage(1_500).unwrap(), (250, Some(1_500)));
        assert_eq!(db.cost_window_usage(1_501).unwrap(), (0, None));
    }
}
