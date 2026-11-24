//! `TaskStore` on SQLite: tasks and their dependency edges.

use aaf_domain::blocked::BlockedReason;
use aaf_domain::budget::RootBudget;
use aaf_domain::dag;
use aaf_domain::event::FactoryEvent;
use aaf_domain::ports::{PortError, PortResult, TaskStore};
use aaf_domain::task::{Priority, Task, TaskState};
use rusqlite::{Connection, OptionalExtension, Params, Row, params};

use super::{Db, corrupt, sql_err, to_i64};

const TASK_COLS_V3: &str = "id, repo_root, parent_id, root_id, depth, title, spec, context_notes, \
     state, priority, fingerprint, blocked_reason, unblock_cycles, created_at, updated_at";
const TASK_COLS_V4: &str = "id, repo_root, parent_id, root_id, depth, title, spec, context_notes, \
     state, priority, fingerprint, blocked_reason, unblock_cycles, created_at, updated_at, \
     scope_paths";

/// Priority first (`P0` sorts lowest), then oldest first; `rowid` keeps
/// insertion order for tasks created in the same second.
const TASK_ORDER: &str = " ORDER BY priority, created_at, rowid";

fn task_cols(has_scope_paths: bool) -> &'static str {
    if has_scope_paths {
        TASK_COLS_V4
    } else {
        TASK_COLS_V3
    }
}

fn to_json<T: serde::Serialize>(value: &T, what: &str) -> PortResult<String> {
    serde_json::to_string(value).map_err(|e| PortError::Invalid(format!("{what}: {e}")))
}

fn from_json<T: serde::de::DeserializeOwned>(text: &str, what: &str) -> PortResult<T> {
    serde_json::from_str(text).map_err(|_| corrupt(what, text))
}

fn task_from_row(row: &Row<'_>, has_scope_paths: bool) -> PortResult<Task> {
    let spec: String = row.get(6).map_err(sql_err)?;
    let notes: String = row.get(7).map_err(sql_err)?;
    let state: String = row.get(8).map_err(sql_err)?;
    let priority: String = row.get(9).map_err(sql_err)?;
    let blocked: Option<String> = row.get(11).map_err(sql_err)?;
    let scope_paths = if has_scope_paths {
        let text: String = row.get(15).map_err(sql_err)?;
        from_json(&text, "tasks.scope_paths")?
    } else {
        Vec::new()
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
        created_at: row.get(13).map_err(sql_err)?,
        updated_at: row.get(14).map_err(sql_err)?,
    })
}

fn query_tasks(
    conn: &Connection,
    sql: &str,
    params: impl Params,
    has_scope_paths: bool,
) -> PortResult<Vec<Task>> {
    let mut stmt = conn.prepare_cached(sql).map_err(sql_err)?;
    let mut rows = stmt.query(params).map_err(sql_err)?;
    let mut tasks = Vec::new();
    while let Some(row) = rows.next().map_err(sql_err)? {
        tasks.push(task_from_row(row, has_scope_paths)?);
    }
    Ok(tasks)
}

fn task_not_found(id: &str) -> PortError {
    PortError::NotFound(format!("task {id}"))
}

fn require_task(conn: &Connection, id: &str) -> PortResult<()> {
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
        let task = task.clone();
        let spec = to_json(&task.spec, "spec")?;
        let notes = to_json(&task.context_notes, "context_notes")?;
        let blocked = match &task.blocked_reason {
            Some(reason) => Some(to_json(reason, "blocked_reason")?),
            None => None,
        };
        let scope_paths = to_json(&task.scope_paths, "scope_paths")?;
        self.write(move |tx| {
            tx.prepare_cached(
                "INSERT INTO tasks (id, repo_root, parent_id, root_id, depth, title, spec, \
                 context_notes, state, priority, fingerprint, blocked_reason, unblock_cycles, \
                 created_at, updated_at, scope_paths) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            )
            .and_then(|mut stmt| {
                stmt.execute(params![
                    task.id,
                    task.repo_root,
                    task.parent_id,
                    task.root_id,
                    task.depth,
                    task.title,
                    spec,
                    notes,
                    task.state.as_str(),
                    task.priority.as_str(),
                    task.fingerprint,
                    blocked,
                    task.unblock_cycles,
                    task.created_at,
                    task.updated_at,
                    scope_paths,
                ])
            })
            .map_err(sql_err)?;
            Ok(())
        })
    }

    fn get_task(&self, id: &str) -> PortResult<Task> {
        let id = id.to_string();
        let has_scope_paths = self.schema_version() >= 4;
        let cols = task_cols(has_scope_paths);
        self.read(move |conn| {
            let sql = format!("SELECT {cols} FROM tasks WHERE id = ?1");
            query_tasks(conn, &sql, params![id], has_scope_paths)?
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
        let has_scope_paths = self.schema_version() >= 4;
        let cols = task_cols(has_scope_paths);
        let mut sql = format!("SELECT {cols} FROM tasks");
        if !clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&clauses.join(" AND "));
        }
        sql.push_str(TASK_ORDER);
        self.read(move |conn| {
            query_tasks(
                conn,
                &sql,
                rusqlite::params_from_iter(args),
                has_scope_paths,
            )
        })
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
            tx.prepare_cached(
                "UPDATE tasks SET state = ?1, blocked_reason = ?2, updated_at = ?3 WHERE id = ?4",
            )
            .and_then(|mut stmt| stmt.execute(params![state.as_str(), blocked, now, id]))
            .map_err(sql_err)?;
            Ok(())
        })
    }

    fn append_context_note(&self, id: &str, note: &str, now: i64) -> PortResult<()> {
        let id = id.to_string();
        let note = note.to_string();
        self.write(move |tx| {
            let stored: Option<String> = tx
                .prepare_cached("SELECT context_notes FROM tasks WHERE id = ?1")
                .and_then(|mut stmt| stmt.query_row(params![id], |row| row.get(0)).optional())
                .map_err(sql_err)?;
            let stored = stored.ok_or_else(|| task_not_found(&id))?;
            let mut notes: Vec<String> = from_json(&stored, "tasks.context_notes")?;
            notes.push(note);
            let notes = to_json(&notes, "context_notes")?;
            tx.prepare_cached("UPDATE tasks SET context_notes = ?1, updated_at = ?2 WHERE id = ?3")
                .and_then(|mut stmt| stmt.execute(params![notes, now, id]))
                .map_err(sql_err)?;
            Ok(())
        })
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
        let has_scope_paths = self.schema_version() >= 4;
        let cols = task_cols(has_scope_paths);
        self.read(move |conn| {
            let sql = format!("SELECT {cols} FROM tasks WHERE parent_id = ?1{TASK_ORDER}");
            query_tasks(conn, &sql, params![parent_id], has_scope_paths)
        })
    }
}

impl Db {
    /// Atomically persists a split (root budget if absent, child tasks, DAG
    /// edges, creation events, and parent context note) in one `BEGIN IMMEDIATE`
    /// transaction. Rejects with [`PortError::Conflict`] if `parent_id` already
    /// has subtasks.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_split_atomic(
        &self,
        parent_id: &str,
        budget: &RootBudget,
        children: &[Task],
        deps: &[(String, String)],
        events: &[FactoryEvent],
        parent_note: &str,
        now: i64,
    ) -> PortResult<()> {
        let parent_id = parent_id.to_string();
        let budget = budget.clone();
        let tokens_used = to_i64(budget.tokens_used, "tokens_used")?;
        let token_limit = to_i64(budget.token_limit, "token_limit")?;
        let wall_secs_used = to_i64(budget.wall_secs_used, "wall_secs_used")?;
        let wall_secs_limit = to_i64(budget.wall_secs_limit, "wall_secs_limit")?;
        let mut encoded_children = Vec::with_capacity(children.len());
        for child in children {
            let spec = to_json(&child.spec, "spec")?;
            let notes = to_json(&child.context_notes, "context_notes")?;
            let blocked = match &child.blocked_reason {
                Some(reason) => Some(to_json(reason, "blocked_reason")?),
                None => None,
            };
            let scope_paths = to_json(&child.scope_paths, "scope_paths")?;
            encoded_children.push((child.clone(), spec, notes, blocked, scope_paths));
        }
        let deps = deps.to_vec();
        let events = events.to_vec();
        let parent_note = parent_note.to_string();

        self.write(move |tx| {
            let parent_state: Option<String> = tx
                .prepare_cached("SELECT state FROM tasks WHERE id = ?1")
                .and_then(|mut stmt| {
                    stmt.query_row(params![parent_id], |row| row.get(0))
                        .optional()
                })
                .map_err(sql_err)?;
            let parent_state = parent_state.ok_or_else(|| task_not_found(&parent_id))?;
            let parent_state = TaskState::parse(&parent_state)
                .ok_or_else(|| corrupt("tasks.state", &parent_state))?;
            if !matches!(parent_state, TaskState::Ready | TaskState::Backlog) {
                return Err(PortError::Invalid(format!(
                    "task {parent_id} in state {} cannot be split (must be ready or backlog)",
                    parent_state.as_str()
                )));
            }
            let has_children = tx
                .prepare_cached("SELECT 1 FROM tasks WHERE parent_id = ?1 LIMIT 1")
                .and_then(|mut stmt| stmt.exists(params![parent_id]))
                .map_err(sql_err)?;
            if has_children {
                return Err(PortError::Conflict(format!(
                    "task {parent_id} already has subtasks"
                )));
            }
            tx.prepare_cached(
                "INSERT OR IGNORE INTO root_budgets (root_id, tokens_used, token_limit, \
                 wall_secs_used, wall_secs_limit, cycles_used, cycles_limit, frozen) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )
            .and_then(|mut stmt| {
                stmt.execute(params![
                    budget.root_id,
                    tokens_used,
                    token_limit,
                    wall_secs_used,
                    wall_secs_limit,
                    budget.cycles_used,
                    budget.cycles_limit,
                    budget.frozen,
                ])
            })
            .map_err(sql_err)?;

            for (task, spec, notes, blocked, scope_paths) in encoded_children {
                tx.prepare_cached(
                    "INSERT INTO tasks (id, repo_root, parent_id, root_id, depth, title, spec, \
                     context_notes, state, priority, fingerprint, blocked_reason, unblock_cycles, \
                     created_at, updated_at, scope_paths) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                )
                .and_then(|mut stmt| {
                    stmt.execute(params![
                        task.id,
                        task.repo_root,
                        task.parent_id,
                        task.root_id,
                        task.depth,
                        task.title,
                        spec,
                        notes,
                        parent_state.as_str(),
                        task.priority.as_str(),
                        task.fingerprint,
                        blocked,
                        task.unblock_cycles,
                        task.created_at,
                        task.updated_at,
                        scope_paths,
                    ])
                })
                .map_err(sql_err)?;
            }

            let mut edges: Vec<(String, String)> = tx
                .prepare_cached("SELECT task_id, blocked_by FROM task_deps")
                .and_then(|mut stmt| {
                    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                        .collect()
                })
                .map_err(sql_err)?;
            for (task_id, blocked_by) in deps {
                require_task(tx, &task_id)?;
                require_task(tx, &blocked_by)?;
                if dag::would_create_cycle(&edges, &task_id, &blocked_by) {
                    return Err(PortError::Conflict(format!(
                        "dependency {task_id} -> {blocked_by} would create a cycle"
                    )));
                }
                tx.prepare_cached(
                    "INSERT OR IGNORE INTO task_deps (task_id, blocked_by) VALUES (?1, ?2)",
                )
                .and_then(|mut stmt| stmt.execute(params![task_id, blocked_by]))
                .map_err(sql_err)?;
                edges.push((task_id, blocked_by));
            }

            for event in events {
                tx.prepare_cached(
                    "INSERT INTO events (task_id, run_id, kind, payload_json, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                )
                .and_then(|mut stmt| {
                    stmt.execute(params![
                        event.task_id,
                        event.run_id,
                        event.kind.as_str(),
                        event.payload_json,
                        event.created_at,
                    ])
                })
                .map_err(sql_err)?;
            }

            let stored: Option<String> = tx
                .prepare_cached("SELECT context_notes FROM tasks WHERE id = ?1")
                .and_then(|mut stmt| {
                    stmt.query_row(params![parent_id], |row| row.get(0))
                        .optional()
                })
                .map_err(sql_err)?;
            let stored = stored.ok_or_else(|| task_not_found(&parent_id))?;
            let mut notes: Vec<String> = from_json(&stored, "tasks.context_notes")?;
            notes.push(parent_note);
            let notes_json = to_json(&notes, "context_notes")?;
            tx.prepare_cached("UPDATE tasks SET context_notes = ?1, updated_at = ?2 WHERE id = ?3")
                .and_then(|mut stmt| stmt.execute(params![notes_json, now, parent_id]))
                .map_err(sql_err)?;
            Ok(())
        })
    }
}
