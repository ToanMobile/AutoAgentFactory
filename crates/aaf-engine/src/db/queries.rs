//! Batched read-only SQLite queries that replace per-task / per-account N+1 loops
//! in `aaf ui`, `aaf cockpit`, and `aaf watch`.

use std::collections::HashMap;

use aaf_domain::blocked::BlockedReason;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{META_FACTORY_FROZEN, PortResult};
use aaf_domain::run::{RunRecord, RunStatus};
use aaf_domain::task::TaskState;
use rusqlite::{OptionalExtension, Row, params};

use super::{Db, corrupt, sql_err, to_u64};

/// Batched board metadata loaded in a single read closure for `aaf ui` `/api/board`.
#[derive(Debug, Clone, Default)]
pub struct BoardEnrichment {
    pub frozen: bool,
    pub deps_by_task: HashMap<String, Vec<String>>,
    pub latest_run_by_task: HashMap<String, String>,
}

/// One `runs` row, columns in table order (shared with `stores.rs`).
pub(super) fn run_from_row(row: &Row<'_>) -> PortResult<RunRecord> {
    let status: String = row.get(6).map_err(sql_err)?;
    Ok(RunRecord {
        id: row.get(0).map_err(sql_err)?,
        task_id: row.get(1).map_err(sql_err)?,
        account_id: row.get(2).map_err(sql_err)?,
        executor: row.get(3).map_err(sql_err)?,
        worktree_path: row.get(4).map_err(sql_err)?,
        branch: row.get(5).map_err(sql_err)?,
        status: RunStatus::parse(&status).ok_or_else(|| corrupt("runs.status", &status))?,
        exit_code: row.get(7).map_err(sql_err)?,
        tokens_in: to_u64(row.get(8).map_err(sql_err)?, "runs.tokens_in")?,
        tokens_out: to_u64(row.get(9).map_err(sql_err)?, "runs.tokens_out")?,
        started_at: row.get(10).map_err(sql_err)?,
        ended_at: row.get(11).map_err(sql_err)?,
    })
}

fn event_from_row(row: &Row<'_>) -> PortResult<FactoryEvent> {
    let kind: String = row.get(3).map_err(sql_err)?;
    Ok(FactoryEvent {
        id: Some(row.get(0).map_err(sql_err)?),
        task_id: row.get(1).map_err(sql_err)?,
        run_id: row.get(2).map_err(sql_err)?,
        kind: EventKind::parse(&kind).ok_or_else(|| corrupt("events.kind", &kind))?,
        payload_json: row.get(4).map_err(sql_err)?,
        created_at: row.get(5).map_err(sql_err)?,
    })
}

impl Db {
    /// Task counts grouped by `(state, blocked kind)` without decoding task
    /// rows; the kind is `None` for non-blocked rows and blocked rows without
    /// a reason. Optionally scoped to `repo_root`.
    pub fn task_state_counts(
        &self,
        repo_root: Option<&str>,
    ) -> PortResult<Vec<(TaskState, Option<&'static str>, usize)>> {
        let repo_root = repo_root.map(str::to_string);
        self.read(move |conn| {
            let sql = if repo_root.is_some() {
                "SELECT state, blocked_reason, COUNT(*) FROM tasks WHERE repo_root = ?1 \
                 GROUP BY state, blocked_reason"
            } else {
                "SELECT state, blocked_reason, COUNT(*) FROM tasks GROUP BY state, blocked_reason"
            };
            let mut stmt = conn.prepare_cached(sql).map_err(sql_err)?;
            let mut rows = match &repo_root {
                Some(root) => stmt.query(params![root]),
                None => stmt.query([]),
            }
            .map_err(sql_err)?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().map_err(sql_err)? {
                let state_str: String = row.get(0).map_err(sql_err)?;
                let state = TaskState::parse(&state_str)
                    .ok_or_else(|| corrupt("tasks.state", &state_str))?;
                let blocked: Option<String> = row.get(1).map_err(sql_err)?;
                let kind = match blocked {
                    Some(text) if state == TaskState::Blocked => Some(
                        serde_json::from_str::<BlockedReason>(&text)
                            .map_err(|_| corrupt("tasks.blocked_reason", &text))?
                            .kind(),
                    ),
                    _ => None,
                };
                let count: i64 = row.get(2).map_err(sql_err)?;
                let count = usize::try_from(count)
                    .map_err(|_| corrupt("tasks.count", &count.to_string()))?;
                out.push((state, kind, count));
            }
            Ok(out)
        })
    }

    /// Loads factory freeze state, per-task dependencies (ordered by `rowid`),
    /// and latest run id per task (`max_by_key(|r| (r.started_at, r.id.clone()))`)
    /// inside a single read closure.
    pub fn board_enrichment(&self, repo_root: Option<&str>) -> PortResult<BoardEnrichment> {
        let repo_root = repo_root.map(str::to_string);
        self.read(move |conn| {
            let meta_val = |key: &str| -> PortResult<Option<String>> {
                conn.prepare_cached("SELECT value FROM meta WHERE key = ?1")
                    .and_then(|mut stmt| {
                        stmt.query_row(params![key], |row| row.get::<_, String>(0))
                            .optional()
                    })
                    .map_err(sql_err)
            };
            let global_frozen = meta_val(META_FACTORY_FROZEN)?.as_deref() == Some("1");
            let frozen = if let Some(root) = repo_root.as_deref() {
                let key = crate::unblocker::ladder::repo_frozen_key(root);
                match meta_val(&key)?.as_deref() {
                    Some("1") => true,
                    Some("probe") => false,
                    _ => global_frozen,
                }
            } else {
                global_frozen
            };

            let mut deps_by_task: HashMap<String, Vec<String>> = HashMap::new();
            let mut best_run: HashMap<String, (i64, String)> = HashMap::new();

            if let Some(root) = repo_root {
                let mut dep_stmt = conn
                    .prepare_cached(
                        "SELECT d.task_id, d.blocked_by FROM task_deps d \
                         INNER JOIN tasks t ON t.id = d.task_id \
                         WHERE t.repo_root = ?1 ORDER BY d.rowid",
                    )
                    .map_err(sql_err)?;
                let mut dep_rows = dep_stmt.query(params![&root]).map_err(sql_err)?;
                while let Some(row) = dep_rows.next().map_err(sql_err)? {
                    let task_id: String = row.get(0).map_err(sql_err)?;
                    let blocked_by: String = row.get(1).map_err(sql_err)?;
                    deps_by_task.entry(task_id).or_default().push(blocked_by);
                }

                let mut run_stmt = conn
                    .prepare_cached(
                        "SELECT r.task_id, r.started_at, r.id FROM runs r \
                         INNER JOIN tasks t ON t.id = r.task_id \
                         WHERE t.repo_root = ?1",
                    )
                    .map_err(sql_err)?;
                let mut run_rows = run_stmt.query(params![&root]).map_err(sql_err)?;
                while let Some(row) = run_rows.next().map_err(sql_err)? {
                    record_latest_run(&mut best_run, row)?;
                }
            } else {
                let mut dep_stmt = conn
                    .prepare_cached("SELECT task_id, blocked_by FROM task_deps ORDER BY rowid")
                    .map_err(sql_err)?;
                let mut dep_rows = dep_stmt.query([]).map_err(sql_err)?;
                while let Some(row) = dep_rows.next().map_err(sql_err)? {
                    let task_id: String = row.get(0).map_err(sql_err)?;
                    let blocked_by: String = row.get(1).map_err(sql_err)?;
                    deps_by_task.entry(task_id).or_default().push(blocked_by);
                }

                let mut run_stmt = conn
                    .prepare_cached("SELECT task_id, started_at, id FROM runs")
                    .map_err(sql_err)?;
                let mut run_rows = run_stmt.query([]).map_err(sql_err)?;
                while let Some(row) = run_rows.next().map_err(sql_err)? {
                    record_latest_run(&mut best_run, row)?;
                }
            }

            let latest_run_by_task = best_run
                .into_iter()
                .map(|(task_id, (_, run_id))| (task_id, run_id))
                .collect();

            Ok(BoardEnrichment {
                frozen,
                deps_by_task,
                latest_run_by_task,
            })
        })
    }

    /// Returns all runs currently in `running` status, optionally scoped to `repo_root`.
    pub fn running_runs(&self, repo_root: Option<&str>) -> PortResult<Vec<RunRecord>> {
        let repo_root = repo_root.map(str::to_string);
        self.read(move |conn| {
            let mut runs = Vec::new();
            if let Some(root) = repo_root {
                let mut stmt = conn
                    .prepare_cached(
                        "SELECT r.id, r.task_id, r.account_id, r.executor, r.worktree_path, \
                         r.branch, r.status, r.exit_code, r.tokens_in, r.tokens_out, \
                         r.started_at, r.ended_at FROM runs r \
                         INNER JOIN tasks t ON t.id = r.task_id \
                         WHERE r.status = 'running' AND t.repo_root = ?1 \
                         ORDER BY r.started_at, r.rowid",
                    )
                    .map_err(sql_err)?;
                let mut rows = stmt.query(params![root]).map_err(sql_err)?;
                while let Some(row) = rows.next().map_err(sql_err)? {
                    runs.push(run_from_row(row)?);
                }
            } else {
                let mut stmt = conn
                    .prepare_cached(
                        "SELECT id, task_id, account_id, executor, worktree_path, branch, \
                         status, exit_code, tokens_in, tokens_out, started_at, ended_at \
                         FROM runs WHERE status = 'running' ORDER BY started_at, rowid",
                    )
                    .map_err(sql_err)?;
                let mut rows = stmt.query([]).map_err(sql_err)?;
                while let Some(row) = rows.next().map_err(sql_err)? {
                    runs.push(run_from_row(row)?);
                }
            }
            Ok(runs)
        })
    }

    /// Events with `id > after_id`, oldest first, at most `limit`, optionally
    /// scoped to tasks under `repo_root` (always including global
    /// `factory_frozen` / `factory_unfrozen` events): a cursor-driven tail of
    /// the log for consumers that must see every event once (`aaf notify`).
    pub fn events_after(
        &self,
        repo_root: Option<&str>,
        after_id: i64,
        limit: u32,
    ) -> PortResult<Vec<FactoryEvent>> {
        let repo_root = repo_root.map(str::to_string);
        self.read(move |conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT e.id, e.task_id, e.run_id, e.kind, e.payload_json, e.created_at \
                     FROM events e LEFT JOIN tasks t ON t.id = e.task_id \
                     WHERE e.id > ?1 AND (?2 IS NULL OR t.repo_root = ?2 OR e.kind IN (?4, ?5)) \
                     ORDER BY e.id ASC LIMIT ?3",
                )
                .map_err(sql_err)?;
            let mut rows = stmt
                .query(params![
                    after_id,
                    repo_root,
                    limit,
                    EventKind::FactoryFrozen.as_str(),
                    EventKind::FactoryUnfrozen.as_str(),
                ])
                .map_err(sql_err)?;
            let mut events = Vec::new();
            while let Some(row) = rows.next().map_err(sql_err)? {
                events.push(event_from_row(row)?);
            }
            Ok(events)
        })
    }

    /// Highest event id, optionally scoped to tasks under `repo_root`
    /// (always including global `factory_frozen` / `factory_unfrozen` events;
    /// `0` when there is none).
    pub fn max_event_id(&self, repo_root: Option<&str>) -> PortResult<i64> {
        let repo_root = repo_root.map(str::to_string);
        self.read(move |conn| {
            conn.query_row(
                "SELECT COALESCE(MAX(e.id), 0) FROM events e LEFT JOIN tasks t \
                 ON t.id = e.task_id WHERE ?1 IS NULL OR t.repo_root = ?1 OR e.kind IN (?2, ?3)",
                params![
                    repo_root,
                    EventKind::FactoryFrozen.as_str(),
                    EventKind::FactoryUnfrozen.as_str(),
                ],
                |row| row.get(0),
            )
            .map_err(sql_err)
        })
    }

    /// Returns up to `limit` most recent events (`ORDER BY created_at DESC, id DESC`),
    /// optionally scoped to tasks under `repo_root`.
    pub fn recent_events(
        &self,
        repo_root: Option<&str>,
        limit: u32,
    ) -> PortResult<Vec<FactoryEvent>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let repo_root = repo_root.map(str::to_string);
        self.read(move |conn| {
            let mut events = Vec::new();
            if let Some(root) = repo_root {
                let mut stmt = conn
                    .prepare_cached(
                        "SELECT e.id, e.task_id, e.run_id, e.kind, e.payload_json, e.created_at \
                         FROM events e INNER JOIN tasks t ON t.id = e.task_id \
                         WHERE t.repo_root = ?1 ORDER BY e.created_at DESC, e.id DESC LIMIT ?2",
                    )
                    .map_err(sql_err)?;
                let mut rows = stmt.query(params![root, limit]).map_err(sql_err)?;
                while let Some(row) = rows.next().map_err(sql_err)? {
                    events.push(event_from_row(row)?);
                }
            } else {
                let mut stmt = conn
                    .prepare_cached(
                        "SELECT id, task_id, run_id, kind, payload_json, created_at \
                         FROM events ORDER BY created_at DESC, id DESC LIMIT ?1",
                    )
                    .map_err(sql_err)?;
                let mut rows = stmt.query(params![limit]).map_err(sql_err)?;
                while let Some(row) = rows.next().map_err(sql_err)? {
                    events.push(event_from_row(row)?);
                }
            }
            Ok(events)
        })
    }

    /// Sums `tokens_in + tokens_out` since `since` grouped by `account_id` in one query.
    pub fn tokens_since_all(&self, since: i64) -> PortResult<HashMap<String, u64>> {
        self.read(move |conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT account_id, COALESCE(SUM(tokens_in + tokens_out), 0) FROM runs \
                     WHERE account_id IS NOT NULL AND started_at >= ?1 GROUP BY account_id",
                )
                .map_err(sql_err)?;
            let mut rows = stmt.query(params![since]).map_err(sql_err)?;
            let mut totals = HashMap::new();
            while let Some(row) = rows.next().map_err(sql_err)? {
                let account_id: String = row.get(0).map_err(sql_err)?;
                let total = to_u64(row.get(1).map_err(sql_err)?, "sum of runs tokens")?;
                totals.insert(account_id, total);
            }
            Ok(totals)
        })
    }

    /// Counts explicit failing `CrossAudit` events for `task_id` since the
    /// latest `ReworkRequested` or human-unblock `TaskStateChanged` event,
    /// scanning newest-first with no 50-event window cap.
    pub fn audit_fails_since_reset(&self, task_id: &str) -> PortResult<u32> {
        let task_id = task_id.to_string();
        self.read(move |conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT kind, payload_json FROM events \
                     WHERE task_id = ?1 AND kind IN (?2, ?3, ?4) \
                     ORDER BY id DESC",
                )
                .map_err(sql_err)?;
            let mut rows = stmt
                .query(params![
                    task_id,
                    EventKind::CrossAudit.as_str(),
                    EventKind::ReworkRequested.as_str(),
                    EventKind::TaskStateChanged.as_str(),
                ])
                .map_err(sql_err)?;
            let mut count: u32 = 0;
            while let Some(row) = rows.next().map_err(sql_err)? {
                let kind: String = row.get(0).map_err(sql_err)?;
                if kind == EventKind::ReworkRequested.as_str() {
                    break;
                }
                let payload: String = row.get(1).map_err(sql_err)?;
                if kind == EventKind::TaskStateChanged.as_str() {
                    if is_human_unblock_payload(&payload) {
                        break;
                    }
                    continue;
                }
                if is_cross_audit_fail_payload(&payload) {
                    count = count.saturating_add(1);
                }
            }
            Ok(count)
        })
    }
}

fn is_human_unblock_payload(payload_json: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(payload_json)
        .ok()
        .is_some_and(|p| {
            p["to"].as_str() == Some("ready")
                && p["why"]
                    .as_str()
                    .is_some_and(|why| why == "unblocked" || why.ends_with(" unblock"))
        })
}

fn is_cross_audit_fail_payload(payload_json: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(payload_json)
        .ok()
        .is_some_and(|p| {
            p["verdict"]
                .as_str()
                .is_some_and(|v| v == "fail" || v.starts_with("fail:"))
        })
}

fn record_latest_run(best: &mut HashMap<String, (i64, String)>, row: &Row<'_>) -> PortResult<()> {
    let task_id: String = row.get(0).map_err(sql_err)?;
    let started_at: i64 = row.get(1).map_err(sql_err)?;
    let run_id: String = row.get(2).map_err(sql_err)?;
    let replace = match best.get(&task_id) {
        None => true,
        Some((prev_ts, prev_id)) => (started_at, run_id.as_str()) >= (*prev_ts, prev_id.as_str()),
    };
    if replace {
        best.insert(task_id, (started_at, run_id));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use aaf_domain::ports::{EventSink, KvStore, RunStore, TaskStore};
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
        status: RunStatus,
        started_at: i64,
        tokens: (u64, u64),
    ) -> RunRecord {
        RunRecord {
            id: id.into(),
            task_id: task_id.into(),
            account_id: account_id.map(str::to_string),
            executor: "claude".into(),
            worktree_path: String::new(),
            branch: "b".into(),
            status,
            exit_code: None,
            tokens_in: tokens.0,
            tokens_out: tokens.1,
            started_at,
            ended_at: None,
        }
    }

    #[test]
    fn board_enrichment_batches_frozen_deps_and_latest_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
        db.set_meta(META_FACTORY_FROZEN, "1").unwrap();
        db.insert_task(&make_task("t1", "/repo-a")).unwrap();
        db.insert_task(&make_task("t2", "/repo-a")).unwrap();
        db.insert_task(&make_task("t3", "/repo-a")).unwrap();
        db.insert_task(&make_task("tb", "/repo-b")).unwrap();
        // Insert deps in non-alphabetical order to confirm rowid preservation.
        db.add_dep("t1", "t3").unwrap();
        db.add_dep("t1", "t2").unwrap();
        db.insert_run(&make_run(
            "r-a",
            "t1",
            Some("acc1"),
            RunStatus::Succeeded,
            200,
            (10, 5),
        ))
        .unwrap();
        db.insert_run(&make_run(
            "r-z",
            "t1",
            Some("acc1"),
            RunStatus::Failed,
            200,
            (1, 1),
        ))
        .unwrap();
        db.insert_run(&make_run(
            "r-b",
            "tb",
            Some("acc2"),
            RunStatus::Running,
            300,
            (50, 25),
        ))
        .unwrap();

        let scoped = db.board_enrichment(Some("/repo-a")).unwrap();
        assert!(scoped.frozen);
        assert_eq!(
            scoped.deps_by_task.get("t1").map(Vec::as_slice),
            Some(&["t3".to_string(), "t2".to_string()][..])
        );
        // Same started_at -> lexicographically larger id wins, matching max_by_key.
        assert_eq!(
            scoped.latest_run_by_task.get("t1").map(String::as_str),
            Some("r-z")
        );
        assert!(!scoped.latest_run_by_task.contains_key("tb"));

        let all = db.board_enrichment(None).unwrap();
        assert_eq!(
            all.latest_run_by_task.get("tb").map(String::as_str),
            Some("r-b")
        );
    }

    #[test]
    fn running_runs_recent_events_and_tokens_since_all_respect_scope_and_window() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
        db.insert_task(&make_task("ta", "/repo-a")).unwrap();
        db.insert_task(&make_task("tb", "/repo-b")).unwrap();
        db.insert_run(&make_run(
            "r1",
            "ta",
            Some("acc1"),
            RunStatus::Running,
            500,
            (100, 50),
        ))
        .unwrap();
        db.insert_run(&make_run(
            "r2",
            "ta",
            Some("acc1"),
            RunStatus::Succeeded,
            100,
            (999, 1),
        ))
        .unwrap();
        db.insert_run(&make_run(
            "r3",
            "tb",
            Some("acc2"),
            RunStatus::Running,
            600,
            (40, 60),
        ))
        .unwrap();

        let run_a = db.running_runs(Some("/repo-a")).unwrap();
        assert_eq!(run_a.len(), 1);
        assert_eq!(run_a[0].id, "r1");
        let run_all = db.running_runs(None).unwrap();
        assert_eq!(run_all.len(), 2);

        let tokens = db.tokens_since_all(400).unwrap();
        assert_eq!(tokens.get("acc1"), Some(&150));
        assert_eq!(tokens.get("acc2"), Some(&100));

        db.emit(&FactoryEvent::new(EventKind::TaskCreated, "{}", 10).for_task("ta"))
            .unwrap();
        db.emit(&FactoryEvent::new(EventKind::TaskStateChanged, "{}", 30).for_task("ta"))
            .unwrap();
        db.emit(&FactoryEvent::new(EventKind::TaskCreated, "{}", 20).for_task("tb"))
            .unwrap();

        let ev_a = db.recent_events(Some("/repo-a"), 1).unwrap();
        assert_eq!(ev_a.len(), 1);
        assert_eq!(ev_a[0].created_at, 30);
        let ev_all = db.recent_events(None, 2).unwrap();
        assert_eq!(ev_all.len(), 2);
        assert_eq!(ev_all[0].created_at, 30);
        assert_eq!(ev_all[1].created_at, 20);
    }
}
