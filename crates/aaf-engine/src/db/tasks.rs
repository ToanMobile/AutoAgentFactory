//! `TaskStore` on SQLite: tasks and their dependency edges.

use aaf_domain::blocked::BlockedReason;
use aaf_domain::dag;
use aaf_domain::event::EventKind;
use aaf_domain::ports::{PortError, PortResult, TaskStore};
use aaf_domain::task::{Priority, Task, TaskState};
use rusqlite::{Connection, OptionalExtension, Params, Row, params};

use super::{Db, corrupt, sql_err};

const MAX_AUTO_RECOVER_COUNT: u32 = aaf_domain::engine_contracts::INFRA_RETRY_MAX;

const TASK_COLS_V3: &str = "id, repo_root, parent_id, root_id, depth, title, spec, context_notes, \
     state, priority, fingerprint, blocked_reason, unblock_cycles, created_at, updated_at";
const TASK_COLS_V4: &str = "id, repo_root, parent_id, root_id, depth, title, spec, context_notes, \
     state, priority, fingerprint, blocked_reason, unblock_cycles, created_at, updated_at, \
     scope_paths";
const TASK_COLS_V5: &str = "id, repo_root, parent_id, root_id, depth, title, spec, context_notes, \
     state, priority, fingerprint, blocked_reason, unblock_cycles, created_at, updated_at, \
     scope_paths, retry_after, auto_recover_count";

/// Priority first (`P0` sorts lowest), then oldest first; `rowid` keeps
/// insertion order for tasks created in the same second.
const TASK_ORDER: &str = " ORDER BY priority, created_at, rowid";

fn task_cols(schema_version: i64) -> &'static str {
    if schema_version >= 5 {
        TASK_COLS_V5
    } else if schema_version >= 4 {
        TASK_COLS_V4
    } else {
        TASK_COLS_V3
    }
}

pub(super) fn to_json<T: serde::Serialize>(value: &T, what: &str) -> PortResult<String> {
    serde_json::to_string(value).map_err(|e| PortError::Invalid(format!("{what}: {e}")))
}

fn from_json<T: serde::de::DeserializeOwned>(text: &str, what: &str) -> PortResult<T> {
    serde_json::from_str(text).map_err(|_| corrupt(what, text))
}

fn task_from_row(row: &Row<'_>, schema_version: i64) -> PortResult<Task> {
    let spec: String = row.get(6).map_err(sql_err)?;
    let notes: String = row.get(7).map_err(sql_err)?;
    let state: String = row.get(8).map_err(sql_err)?;
    let priority: String = row.get(9).map_err(sql_err)?;
    let blocked: Option<String> = row.get(11).map_err(sql_err)?;
    let scope_paths = if schema_version >= 4 {
        let text: String = row.get(15).map_err(sql_err)?;
        from_json(&text, "tasks.scope_paths")?
    } else {
        Vec::new()
    };
    let (retry_after, auto_recover_count) = if schema_version >= 5 {
        (row.get(16).map_err(sql_err)?, row.get(17).map_err(sql_err)?)
    } else {
        (None, 0)
    };
    Ok(Task {
        id: row.get(0).map_err(sql_err)?,
        repo_root: row.get(1).map_err(sql_err)?,
        parent_id: row.get(2).map_err(sql_err)?,
        root_id: row.get(3).map_err(sql_err)?,
        depth: row.get(4).map_err(sql_err)?,
        title: row.get(5).map_err(sql_err)?,
        spec: from_json(&spec, "tasks.spec")?,
        context_notes: from_json(&notes, "tasks.context_notes")?,
        state: TaskState::parse(&state).ok_or_else(|| corrupt("tasks.state", &state))?,
        priority: Priority::parse(&priority).ok_or_else(|| corrupt("tasks.priority", &priority))?,
        fingerprint: row.get(10).map_err(sql_err)?,
        blocked_reason: match blocked {
            Some(text) => Some(from_json(&text, "tasks.blocked_reason")?),
            None => None,
        },
        unblock_cycles: row.get(12).map_err(sql_err)?,
        scope_paths,
        retry_after,
        auto_recover_count,
        created_at: row.get(13).map_err(sql_err)?,
        updated_at: row.get(14).map_err(sql_err)?,
    })
}

fn query_tasks(
    conn: &Connection,
    sql: &str,
    params: impl Params,
    schema_version: i64,
) -> PortResult<Vec<Task>> {
    let mut stmt = conn.prepare_cached(sql).map_err(sql_err)?;
    let mut rows = stmt.query(params).map_err(sql_err)?;
    let mut tasks = Vec::new();
    while let Some(row) = rows.next().map_err(sql_err)? {
        tasks.push(task_from_row(row, schema_version)?);
    }
    Ok(tasks)
}

pub(super) struct EncodedTask {
    pub(super) task: Task,
    spec: String,
    notes: String,
    blocked: Option<String>,
    scope_paths: String,
}

pub(super) fn encode_task(task: &Task) -> PortResult<EncodedTask> {
    Ok(EncodedTask {
        spec: to_json(&task.spec, "spec")?,
        notes: to_json(&task.context_notes, "context_notes")?,
        blocked: task
            .blocked_reason
            .as_ref()
            .map(|r| to_json(r, "blocked_reason"))
            .transpose()?,
        scope_paths: to_json(&task.scope_paths, "scope_paths")?,
        task: task.clone(),
    })
}

pub(super) fn insert_task_row(
    conn: &Connection,
    enc: &EncodedTask,
    state_str: &str,
) -> PortResult<()> {
    let t = &enc.task;
    conn.prepare_cached(
        "INSERT INTO tasks (id, repo_root, parent_id, root_id, depth, title, spec, \
         context_notes, state, priority, fingerprint, blocked_reason, unblock_cycles, \
         created_at, updated_at, scope_paths, retry_after, auto_recover_count) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
    )
    .and_then(|mut stmt| {
        stmt.execute(params![
            t.id,
            t.repo_root,
            t.parent_id,
            t.root_id,
            t.depth,
            t.title,
            enc.spec,
            enc.notes,
            state_str,
            t.priority.as_str(),
            t.fingerprint,
            enc.blocked,
            t.unblock_cycles,
            t.created_at,
            t.updated_at,
            enc.scope_paths,
            t.retry_after,
            t.auto_recover_count,
        ])
    })
    .map_err(sql_err)?;
    Ok(())
}

pub(super) fn append_note_tx(
    conn: &Connection,
    id: &str,
    note: String,
    now: i64,
) -> PortResult<()> {
    let stored: Option<String> = conn
        .prepare_cached("SELECT context_notes FROM tasks WHERE id = ?1")
        .and_then(|mut stmt| stmt.query_row(params![id], |row| row.get(0)).optional())
        .map_err(sql_err)?;
    let stored = stored.ok_or_else(|| task_not_found(id))?;
    let mut notes: Vec<String> = from_json(&stored, "tasks.context_notes")?;
    notes.push(note);
    let notes_json = to_json(&notes, "context_notes")?;
    conn.prepare_cached("UPDATE tasks SET context_notes = ?1, updated_at = ?2 WHERE id = ?3")
        .and_then(|mut stmt| stmt.execute(params![notes_json, now, id]))
        .map_err(sql_err)?;
    Ok(())
}

pub(super) fn task_not_found(id: &str) -> PortError {
    PortError::NotFound(format!("task {id}"))
}

pub(super) fn require_task(conn: &Connection, id: &str) -> PortResult<()> {
    let found = conn
        .prepare_cached("SELECT 1 FROM tasks WHERE id = ?1")
        .and_then(|mut stmt| stmt.exists(params![id]))
        .map_err(sql_err)?;
    if found {
        Ok(())
    } else {
        Err(task_not_found(id))
    }
}

/// Turns "no row was updated" into `NotFound`.
fn one_task_changed(changed: usize, id: &str) -> PortResult<()> {
    if changed == 0 {
        Err(task_not_found(id))
    } else {
        Ok(())
    }
}

impl TaskStore for Db {
    fn insert_task(&self, task: &Task) -> PortResult<()> {
        let enc = encode_task(task)?;
        self.write(move |tx| insert_task_row(tx, &enc, enc.task.state.as_str()))
    }

    fn get_task(&self, id: &str) -> PortResult<Task> {
        let id = id.to_string();
        let ver = self.schema_version();
        let cols = task_cols(ver);
        self.read(move |conn| {
            let sql = format!("SELECT {cols} FROM tasks WHERE id = ?1");
            query_tasks(conn, &sql, params![id], ver)?
                .pop()
                .ok_or_else(|| task_not_found(&id))
        })
    }

    fn list_tasks(
        &self,
        repo_root: Option<&str>,
        state: Option<TaskState>,
    ) -> PortResult<Vec<Task>> {
        // Only the fixed fragments below are concatenated; values are bound.
        let mut clauses: Vec<&str> = Vec::new();
        let mut args: Vec<String> = Vec::new();
        if let Some(repo_root) = repo_root {
            clauses.push("repo_root = ?");
            args.push(repo_root.to_string());
        }
        if let Some(state) = state {
            clauses.push("state = ?");
            args.push(state.as_str().to_string());
        }
        let ver = self.schema_version();
        let cols = task_cols(ver);
        let mut sql = format!("SELECT {cols} FROM tasks");
        if !clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&clauses.join(" AND "));
        }
        sql.push_str(TASK_ORDER);
        self.read(move |conn| query_tasks(conn, &sql, rusqlite::params_from_iter(args), ver))
    }

    fn set_state(
        &self,
        id: &str,
        state: TaskState,
        blocked: Option<&BlockedReason>,
        now: i64,
    ) -> PortResult<()> {
        let id = id.to_string();
        let blocked = match (state, blocked) {
            (TaskState::Blocked, Some(reason)) => Some(to_json(reason, "blocked_reason")?),
            _ => None,
        };
        let reset_recover = matches!(
            state,
            TaskState::Done | TaskState::Review | TaskState::Cancelled
        );
        self.write(move |tx| {
            let current: Option<String> = tx
                .prepare_cached("SELECT state FROM tasks WHERE id = ?1")
                .and_then(|mut stmt| stmt.query_row(params![id], |row| row.get(0)).optional())
                .map_err(sql_err)?;
            let current = current.ok_or_else(|| task_not_found(&id))?;
            let current =
                TaskState::parse(&current).ok_or_else(|| corrupt("tasks.state", &current))?;
            if !current.can_transition_to(state) {
                return Err(PortError::Invalid(format!(
                    "task {id}: transition {} -> {} is not allowed",
                    current.as_str(),
                    state.as_str()
                )));
            }
            let sql = if reset_recover {
                "UPDATE tasks SET state = ?1, blocked_reason = ?2, retry_after = NULL, \
                 auto_recover_count = 0, updated_at = ?3 WHERE id = ?4"
            } else {
                "UPDATE tasks SET state = ?1, blocked_reason = ?2, retry_after = NULL, \
                 updated_at = ?3 WHERE id = ?4"
            };
            tx.prepare_cached(sql)
                .and_then(|mut stmt| stmt.execute(params![state.as_str(), blocked, now, id]))
                .map_err(sql_err)?;
            Ok(())
        })
    }

    fn compare_and_set_state(
        &self,
        id: &str,
        expected: TaskState,
        next: TaskState,
        blocked_reason: Option<&BlockedReason>,
        now: i64,
    ) -> PortResult<bool> {
        super::tasks_cas::compare_and_set_state_impl(self, id, expected, next, blocked_reason, now)
    }

    fn append_context_note(&self, id: &str, note: &str, now: i64) -> PortResult<()> {
        let (id, note) = (id.to_string(), note.to_string());
        self.write(move |tx| append_note_tx(tx, &id, note, now))
    }

    fn increment_unblock_cycles(&self, id: &str, now: i64) -> PortResult<u32> {
        let id = id.to_string();
        self.write(move |tx| {
            let changed = tx
                .prepare_cached(
                    "UPDATE tasks SET unblock_cycles = unblock_cycles + 1, updated_at = ?1 \
                     WHERE id = ?2",
                )
                .and_then(|mut stmt| stmt.execute(params![now, id]))
                .map_err(sql_err)?;
            one_task_changed(changed, &id)?;
            tx.prepare_cached("SELECT unblock_cycles FROM tasks WHERE id = ?1")
                .and_then(|mut stmt| stmt.query_row(params![id], |row| row.get(0)))
                .map_err(sql_err)
        })
    }

    fn add_dep(&self, task_id: &str, blocked_by: &str) -> PortResult<()> {
        let task_id = task_id.to_string();
        let blocked_by = blocked_by.to_string();
        self.write(move |tx| {
            require_task(tx, &task_id)?;
            require_task(tx, &blocked_by)?;
            // Edges are read inside the write transaction, so no other writer
            // can slip in the edge that closes the cycle after the check.
            let edges: Vec<(String, String)> = tx
                .prepare_cached("SELECT task_id, blocked_by FROM task_deps")
                .and_then(|mut stmt| {
                    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                        .collect()
                })
                .map_err(sql_err)?;
            if dag::would_create_cycle(&edges, &task_id, &blocked_by) {
                return Err(PortError::Conflict(format!(
                    "dependency {task_id} -> {blocked_by} would create a cycle"
                )));
            }
            // Re-adding an existing edge is a no-op.
            tx.prepare_cached(
                "INSERT OR IGNORE INTO task_deps (task_id, blocked_by) VALUES (?1, ?2)",
            )
            .and_then(|mut stmt| stmt.execute(params![task_id, blocked_by]))
            .map_err(sql_err)?;
            Ok(())
        })
    }

    fn remove_dep(&self, task_id: &str, blocked_by: &str) -> PortResult<bool> {
        let task_id = task_id.to_string();
        let blocked_by = blocked_by.to_string();
        self.write(move |tx| {
            let changed = tx
                .prepare_cached("DELETE FROM task_deps WHERE task_id = ?1 AND blocked_by = ?2")
                .and_then(|mut stmt| stmt.execute(params![task_id, blocked_by]))
                .map_err(sql_err)?;
            Ok(changed > 0)
        })
    }

    fn deps_of(&self, task_id: &str) -> PortResult<Vec<String>> {
        let task_id = task_id.to_string();
        self.read(move |conn| {
            conn.prepare_cached(
                "SELECT blocked_by FROM task_deps WHERE task_id = ?1 ORDER BY rowid",
            )
            .and_then(|mut stmt| {
                stmt.query_map(params![task_id], |row| row.get(0))?
                    .collect()
            })
            .map_err(sql_err)
        })
    }

    fn all_deps(&self) -> PortResult<Vec<(String, String)>> {
        self.read(|conn| {
            conn.prepare_cached(
                "SELECT task_id, blocked_by FROM task_deps ORDER BY task_id, blocked_by",
            )
            .and_then(|mut stmt| {
                stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect()
            })
            .map_err(sql_err)
        })
    }

    fn children_of(&self, parent_id: &str) -> PortResult<Vec<Task>> {
        let parent_id = parent_id.to_string();
        let ver = self.schema_version();
        let cols = task_cols(ver);
        self.read(move |conn| {
            let sql = format!("SELECT {cols} FROM tasks WHERE parent_id = ?1{TASK_ORDER}");
            query_tasks(conn, &sql, params![parent_id], ver)
        })
    }

    fn set_retry_schedule(
        &self,
        id: &str,
        retry_after: Option<i64>,
        auto_recover_count: u32,
        now: i64,
    ) -> PortResult<()> {
        let id = id.to_string();
        self.write(move |tx| {
            let changed = tx
                .prepare_cached(
                    "UPDATE tasks SET retry_after = ?1, auto_recover_count = ?2, updated_at = ?3 \
                     WHERE id = ?4",
                )
                .and_then(|mut stmt| {
                    stmt.execute(params![retry_after, auto_recover_count, now, id])
                })
                .map_err(sql_err)?;
            one_task_changed(changed, &id)
        })
    }

    fn wake_due_blocked_tasks(&self, repo_root: &str, now: i64) -> PortResult<Vec<String>> {
        let repo_root = repo_root.to_string();
        self.write(move |tx| {
            let candidates: Vec<(String, Option<String>)> = tx
                .prepare_cached(
                    "SELECT id, blocked_reason FROM tasks \
                     WHERE repo_root = ?1 AND state = 'blocked' \
                       AND retry_after IS NOT NULL AND retry_after <= ?2 \
                       AND auto_recover_count < ?3 \
                     ORDER BY priority, created_at, rowid",
                )
                .and_then(|mut stmt| {
                    stmt.query_map(params![repo_root, now, MAX_AUTO_RECOVER_COUNT], |row| {
                        Ok((row.get(0)?, row.get(1)?))
                    })?
                    .collect()
                })
                .map_err(sql_err)?;
            let mut woken = Vec::new();
            let payload = r#"{"to":"ready","why":"auto_retry_due"}"#;
            for (id, blocked_raw) in candidates {
                let Some(raw) = blocked_raw else { continue };
                let reason: BlockedReason = from_json(&raw, "tasks.blocked_reason")?;
                if !matches!(
                    reason,
                    BlockedReason::Infra { .. } | BlockedReason::Quota { .. }
                ) {
                    continue;
                }
                let changed = tx
                    .prepare_cached(
                        "UPDATE tasks SET state = 'ready', blocked_reason = NULL, \
                         retry_after = NULL, auto_recover_count = auto_recover_count + 1, \
                         updated_at = ?1 WHERE id = ?2 AND state = 'blocked'",
                    )
                    .and_then(|mut stmt| stmt.execute(params![now, id]))
                    .map_err(sql_err)?;
                if changed == 0 {
                    continue;
                }
                tx.prepare_cached(
                    "INSERT INTO events (task_id, run_id, kind, payload_json, created_at) \
                     VALUES (?1, NULL, ?2, ?3, ?4)",
                )
                .and_then(|mut stmt| {
                    stmt.execute(params![
                        id,
                        EventKind::TaskStateChanged.as_str(),
                        payload,
                        now
                    ])
                })
                .map_err(sql_err)?;
                woken.push(id);
            }
            Ok(woken)
        })
    }
}
