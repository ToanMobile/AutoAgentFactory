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

/// Summary of runs for a task shown directly on `/api/board` cards.
#[derive(Debug, Clone, Default)]
pub struct BoardRunSummary {
    pub id: String,
    pub status: String,
    pub executor: String,
    pub account: Option<String>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub run_count: u32,
    pub total_duration_secs: i64,
}

/// Batched board metadata loaded in a single read closure for `aaf ui` `/api/board`.
#[derive(Debug, Clone, Default)]
pub struct BoardEnrichment {
    pub frozen: bool,
    pub deps_by_task: HashMap<String, Vec<String>>,
    pub latest_run_by_task: HashMap<String, String>,
    pub run_summary_by_task: HashMap<String, BoardRunSummary>,
    pub latest_event_by_task: HashMap<String, (String, i64)>,
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
                let key = aaf_fp::fs_private::repo_frozen_key(root);
                match meta_val(&key)?.as_deref() {
                    Some("1") => true,
                    Some("probe") => false,
                    _ => global_frozen,
                }
            } else {
                global_frozen
            };

            let mut deps_by_task: HashMap<String, Vec<String>> = HashMap::new();
            let mut run_summary_by_task: HashMap<String, BoardRunSummary> = HashMap::new();
            let mut latest_event_by_task: HashMap<String, (String, i64)> = HashMap::new();

            let mut account_names: HashMap<String, String> = HashMap::new();
            if let Ok(mut acc_stmt) = conn.prepare_cached("SELECT id, name FROM accounts")
                && let Ok(mut acc_rows) = acc_stmt.query([])
            {
                while let Ok(Some(row)) = acc_rows.next() {
                    if let (Ok(id), Ok(name)) = (row.get::<_, String>(0), row.get::<_, String>(1))
                        && !name.trim().is_empty()
                    {
                        account_names.insert(id, name);
                    }
                }
            }

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
                        "SELECT r.task_id, r.started_at, r.id, r.status, r.executor, \
                         r.account_id, r.ended_at FROM runs r \
                         INNER JOIN tasks t ON t.id = r.task_id \
                         WHERE t.repo_root = ?1",
                    )
                    .map_err(sql_err)?;
                let mut run_rows = run_stmt.query(params![&root]).map_err(sql_err)?;
                while let Some(row) = run_rows.next().map_err(sql_err)? {
                    record_run_summary(&mut run_summary_by_task, &account_names, row)?;
                }

                let mut ev_stmt = conn
                    .prepare_cached(
                        "SELECT e.task_id, e.kind, e.created_at FROM events e \
                         INNER JOIN tasks t ON t.id = e.task_id \
                         WHERE t.repo_root = ?1 AND t.state IN ('running', 'gating') \
                         AND e.id = (SELECT MAX(e2.id) FROM events e2 WHERE e2.task_id = e.task_id)",
                    )
                    .map_err(sql_err)?;
                let mut ev_rows = ev_stmt.query(params![&root]).map_err(sql_err)?;
                while let Some(row) = ev_rows.next().map_err(sql_err)? {
                    let tid: String = row.get(0).map_err(sql_err)?;
                    let kind: String = row.get(1).map_err(sql_err)?;
                    let ts: i64 = row.get(2).map_err(sql_err)?;
                    latest_event_by_task.insert(tid, (kind, ts));
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
                    .prepare_cached(
                        "SELECT task_id, started_at, id, status, executor, account_id, ended_at \
                         FROM runs",
                    )
                    .map_err(sql_err)?;
                let mut run_rows = run_stmt.query([]).map_err(sql_err)?;
                while let Some(row) = run_rows.next().map_err(sql_err)? {
                    record_run_summary(&mut run_summary_by_task, &account_names, row)?;
                }

                let mut ev_stmt = conn
                    .prepare_cached(
                        "SELECT e.task_id, e.kind, e.created_at FROM events e \
                         INNER JOIN tasks t ON t.id = e.task_id \
                         WHERE t.state IN ('running', 'gating') \
                         AND e.id = (SELECT MAX(e2.id) FROM events e2 WHERE e2.task_id = e.task_id)",
                    )
                    .map_err(sql_err)?;
                let mut ev_rows = ev_stmt.query([]).map_err(sql_err)?;
                while let Some(row) = ev_rows.next().map_err(sql_err)? {
                    let tid: String = row.get(0).map_err(sql_err)?;
                    let kind: String = row.get(1).map_err(sql_err)?;
                    let ts: i64 = row.get(2).map_err(sql_err)?;
                    latest_event_by_task.insert(tid, (kind, ts));
                }
            }

            let latest_run_by_task = run_summary_by_task
                .iter()
                .map(|(task_id, summary)| (task_id.clone(), summary.id.clone()))
                .collect();

            Ok(BoardEnrichment {
                frozen,
                deps_by_task,
                latest_run_by_task,
                run_summary_by_task,
                latest_event_by_task,
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
    /// `factory_frozen` / `factory_unfrozen` / `cost_paused` / `cost_resumed`
    /// events): a cursor-driven tail of the log for consumers that must see
    /// every event once (`aaf notify`).
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
                     WHERE e.id > ?1 AND (?2 IS NULL OR t.repo_root = ?2 OR e.kind IN (?4, ?5, ?6, ?7)) \
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
                    EventKind::CostPaused.as_str(),
                    EventKind::CostResumed.as_str(),
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
    /// (always including global `factory_frozen` / `factory_unfrozen` /
    /// `cost_paused` / `cost_resumed` events; `0` when there is none).
    pub fn max_event_id(&self, repo_root: Option<&str>) -> PortResult<i64> {
        let repo_root = repo_root.map(str::to_string);
        self.read(move |conn| {
            conn.query_row(
                "SELECT COALESCE(MAX(e.id), 0) FROM events e LEFT JOIN tasks t \
                 ON t.id = e.task_id WHERE ?1 IS NULL OR t.repo_root = ?1 OR e.kind IN (?2, ?3, ?4, ?5)",
                params![
                    repo_root,
                    EventKind::FactoryFrozen.as_str(),
                    EventKind::FactoryUnfrozen.as_str(),
                    EventKind::CostPaused.as_str(),
                    EventKind::CostResumed.as_str(),
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

    /// Returns `(repo_root, active_count, ready_count)` grouped by repo without
    /// deserializing full task specs/notes. Excludes archived tasks.
    pub fn repo_task_summary(
        &self,
        repo_root: Option<&str>,
    ) -> PortResult<Vec<(String, usize, usize)>> {
        let repo_root = repo_root.map(str::to_string);
        self.read(move |conn| {
            let mut out = Vec::new();
            let push_row = |row: &rusqlite::Row<'_>, out: &mut Vec<(String, usize, usize)>| -> PortResult<()> {
                let root: String = row.get(0).map_err(sql_err)?;
                let active = usize::try_from(row.get::<_, i64>(1).map_err(sql_err)?.max(0)).unwrap_or(0);
                let ready = usize::try_from(row.get::<_, i64>(2).map_err(sql_err)?.max(0)).unwrap_or(0);
                if !root.trim().is_empty() {
                    out.push((root, active, ready));
                }
                Ok(())
            };
            if let Some(root) = repo_root {
                let mut stmt = conn
                    .prepare_cached(
                        "SELECT repo_root, \
                         COALESCE(SUM(CASE WHEN state NOT IN ('done','cancelled') THEN 1 ELSE 0 END), 0), \
                         COALESCE(SUM(CASE WHEN state = 'ready' THEN 1 ELSE 0 END), 0) \
                         FROM tasks WHERE archived_at IS NULL AND repo_root = ?1 GROUP BY repo_root",
                    )
                    .map_err(sql_err)?;
                let mut rows = stmt.query(params![root]).map_err(sql_err)?;
                while let Some(row) = rows.next().map_err(sql_err)? {
                    push_row(row, &mut out)?;
                }
            } else {
                let mut stmt = conn
                    .prepare_cached(
                        "SELECT repo_root, \
                         COALESCE(SUM(CASE WHEN state NOT IN ('done','cancelled') THEN 1 ELSE 0 END), 0), \
                         COALESCE(SUM(CASE WHEN state = 'ready' THEN 1 ELSE 0 END), 0) \
                         FROM tasks WHERE archived_at IS NULL GROUP BY repo_root",
                    )
                    .map_err(sql_err)?;
                let mut rows = stmt.query([]).map_err(sql_err)?;
                while let Some(row) = rows.next().map_err(sql_err)? {
                    push_row(row, &mut out)?;
                }
            }
            Ok(out)
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

fn record_run_summary(
    summaries: &mut HashMap<String, BoardRunSummary>,
    account_names: &HashMap<String, String>,
    row: &Row<'_>,
) -> PortResult<()> {
    let task_id: String = row.get(0).map_err(sql_err)?;
    let started_at: i64 = row.get(1).map_err(sql_err)?;
    let run_id: String = row.get(2).map_err(sql_err)?;
    let status: String = row.get(3).map_err(sql_err)?;
    let executor: String = row.get(4).map_err(sql_err)?;
    let account_id: Option<String> = row.get(5).map_err(sql_err)?;
    let ended_at: Option<i64> = row.get(6).map_err(sql_err)?;
    let account = account_id.map(|id| account_names.get(&id).cloned().unwrap_or(id));
    let elapsed = ended_at.map(|end| (end - started_at).max(0)).unwrap_or(0);

    match summaries.get_mut(&task_id) {
        None => {
            summaries.insert(
                task_id,
                BoardRunSummary {
                    id: run_id,
                    status,
                    executor,
                    account,
                    started_at,
                    ended_at,
                    run_count: 1,
                    total_duration_secs: elapsed,
                },
            );
        }
        Some(entry) => {
            entry.run_count = entry.run_count.saturating_add(1);
            entry.total_duration_secs = entry.total_duration_secs.saturating_add(elapsed);
            if (started_at, run_id.as_str()) >= (entry.started_at, entry.id.as_str()) {
                entry.id = run_id;
                entry.status = status;
                entry.executor = executor;
                entry.account = account;
                entry.started_at = started_at;
                entry.ended_at = ended_at;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "queries_tests.rs"]
mod tests;
