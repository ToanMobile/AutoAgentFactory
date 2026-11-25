//! Atomic `aaf board cancel` transaction (`BEGIN IMMEDIATE`) on SQLite.

use std::collections::{HashMap, HashSet, VecDeque};

use aaf_domain::event::EventKind;
use aaf_domain::ports::{PortError, PortResult};
use aaf_domain::task::TaskState;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;

use super::{Db, corrupt, sql_err};

/// Successful result of [`Db::cancel_task_atomic`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelOutcome {
    pub task_id: String,
    pub parent_id: Option<String>,
    pub cancelled_ids: Vec<String>,
    pub affected_dependents: Vec<String>,
}

/// Policy refusal returned by [`Db::cancel_task_atomic`] without modifying the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelRefusal {
    ActiveState { task_id: String, state: TaskState },
    AlreadyTerminal { task_id: String, state: TaskState },
    RunningRun { task_id: String, run_id: String },
    ActiveDescendant { child_id: String, state: TaskState },
    RunningDescendantRun { child_id: String, run_id: String },
    HasUnfinishedChildren { children: Vec<String> },
}

fn is_cancellable(state: TaskState) -> bool {
    matches!(
        state,
        TaskState::Backlog | TaskState::Ready | TaskState::Blocked | TaskState::Review
    )
}

fn first_running_run(conn: &Connection, task_id: &str) -> PortResult<Option<String>> {
    conn.prepare_cached("SELECT id FROM runs WHERE task_id = ?1 AND status = 'running' LIMIT 1")
        .and_then(|mut stmt| {
            stmt.query_row(params![task_id], |row| row.get(0))
                .optional()
        })
        .map_err(sql_err)
}

fn task_state_row(conn: &Connection, id: &str) -> PortResult<Option<(TaskState, Option<String>)>> {
    let row: Option<(String, Option<String>)> = conn
        .prepare_cached("SELECT state, parent_id FROM tasks WHERE id = ?1")
        .and_then(|mut stmt| {
            stmt.query_row(params![id], |row| Ok((row.get(0)?, row.get(1)?)))
                .optional()
        })
        .map_err(sql_err)?;
    match row {
        None => Ok(None),
        Some((raw_state, parent_id)) => {
            let state =
                TaskState::parse(&raw_state).ok_or_else(|| corrupt("tasks.state", &raw_state))?;
            Ok(Some((state, parent_id)))
        }
    }
}

fn direct_children(conn: &Connection, parent_id: &str) -> PortResult<Vec<(String, TaskState)>> {
    let rows: Vec<(String, String)> = conn
        .prepare_cached(
            "SELECT id, state FROM tasks WHERE parent_id = ?1 \
             ORDER BY priority, created_at, rowid",
        )
        .and_then(|mut stmt| {
            stmt.query_map(params![parent_id], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect()
        })
        .map_err(sql_err)?;
    let mut out = Vec::with_capacity(rows.len());
    for (id, raw_state) in rows {
        let state =
            TaskState::parse(&raw_state).ok_or_else(|| corrupt("tasks.state", &raw_state))?;
        out.push((id, state));
    }
    Ok(out)
}

fn collect_affected_dependents(
    conn: &Connection,
    cancelled: &HashSet<String>,
) -> PortResult<Vec<String>> {
    let edges: Vec<(String, String)> = conn
        .prepare_cached("SELECT task_id, blocked_by FROM task_deps ORDER BY task_id, blocked_by")
        .and_then(|mut stmt| {
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect()
        })
        .map_err(sql_err)?;
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
    for (task_id, blocked_by) in &edges {
        dependents
            .entry(blocked_by.as_str())
            .or_default()
            .push(task_id.as_str());
    }

    let mut queue: VecDeque<String> = cancelled.iter().cloned().collect();
    let mut visited: HashSet<String> = cancelled.clone();
    let mut affected = Vec::new();
    while let Some(cur) = queue.pop_front() {
        let Some(next_list) = dependents.get(cur.as_str()) else {
            continue;
        };
        for &dep_task in next_list {
            if !visited.insert(dep_task.to_string()) {
                continue;
            }
            let Some((state, _)) = task_state_row(conn, dep_task)? else {
                continue;
            };
            if state.is_terminal() {
                continue;
            }
            affected.push(dep_task.to_string());
            queue.push_back(dep_task.to_string());
        }
    }
    affected.sort();
    affected.dedup();
    Ok(affected)
}

impl Db {
    /// Atomically validates and cancels `id` (and unfinished descendants when
    /// `cascade` is `true`) inside a single `BEGIN IMMEDIATE` transaction.
    pub fn cancel_task_atomic(
        &self,
        id: &str,
        cascade: bool,
        reason: Option<&str>,
        now: i64,
    ) -> PortResult<Result<CancelOutcome, CancelRefusal>> {
        let root_id = id.to_string();
        let reason = reason.map(str::to_string);
        self.write(move |tx| {
            let Some((state, parent_id)) = task_state_row(tx, &root_id)? else {
                return Err(PortError::NotFound(format!("task {root_id}")));
            };
            if matches!(state, TaskState::Running | TaskState::Gating) {
                return Ok(Err(CancelRefusal::ActiveState {
                    task_id: root_id,
                    state,
                }));
            }
            if state.is_terminal() {
                return Ok(Err(CancelRefusal::AlreadyTerminal {
                    task_id: root_id,
                    state,
                }));
            }
            if let Some(run_id) = first_running_run(tx, &root_id)? {
                return Ok(Err(CancelRefusal::RunningRun {
                    task_id: root_id,
                    run_id,
                }));
            }

            let mut queue: VecDeque<String> = VecDeque::from([root_id.clone()]);
            let mut visited: HashSet<String> = HashSet::from([root_id.clone()]);
            let mut unfinished_descendants: Vec<(String, TaskState)> = Vec::new();
            while let Some(parent) = queue.pop_front() {
                for (child_id, child_state) in direct_children(tx, &parent)? {
                    if !visited.insert(child_id.clone()) {
                        continue;
                    }
                    if matches!(child_state, TaskState::Running | TaskState::Gating) {
                        return Ok(Err(CancelRefusal::ActiveDescendant {
                            child_id,
                            state: child_state,
                        }));
                    }
                    if let Some(run_id) = first_running_run(tx, &child_id)? {
                        return Ok(Err(CancelRefusal::RunningDescendantRun {
                            child_id,
                            run_id,
                        }));
                    }
                    if is_cancellable(child_state) {
                        unfinished_descendants.push((child_id.clone(), child_state));
                    }
                    queue.push_back(child_id);
                }
            }

            if !unfinished_descendants.is_empty() && !cascade {
                let children = unfinished_descendants
                    .into_iter()
                    .map(|(child_id, _)| child_id)
                    .collect();
                return Ok(Err(CancelRefusal::HasUnfinishedChildren { children }));
            }

            let mut targets = Vec::with_capacity(1 + unfinished_descendants.len());
            targets.push((root_id.clone(), state, false));
            for (child_id, child_state) in unfinished_descendants {
                targets.push((child_id, child_state, true));
            }

            let mut cancelled_ids = Vec::with_capacity(targets.len());
            let mut cancelled_set = HashSet::with_capacity(targets.len());
            for (tid, old_state, is_cascade_child) in targets {
                let changed = tx
                    .prepare_cached(
                        "UPDATE tasks SET state = 'cancelled', blocked_reason = NULL, \
                         retry_after = NULL, auto_recover_count = 0, updated_at = ?1 \
                         WHERE id = ?2 AND state = ?3",
                    )
                    .and_then(|mut stmt| stmt.execute(params![now, tid, old_state.as_str()]))
                    .map_err(sql_err)?;
                if changed != 1 {
                    return Err(PortError::Conflict(format!(
                        "task {tid} changed state concurrently during cancel"
                    )));
                }
                let why = reason.as_deref().unwrap_or(if is_cascade_child {
                    "aaf board cancel --cascade"
                } else {
                    "aaf board cancel"
                });
                let mut payload = json!({
                    "from": old_state.as_str(),
                    "to": TaskState::Cancelled.as_str(),
                    "why": why,
                    "by": "human",
                });
                if let Some(obj) = payload.as_object_mut() {
                    if let Some(r) = reason.as_deref() {
                        obj.insert("reason".to_string(), json!(r));
                    }
                    if is_cascade_child {
                        obj.insert("cascade_from".to_string(), json!(root_id));
                    }
                }
                tx.prepare_cached(
                    "INSERT INTO events (task_id, run_id, kind, payload_json, created_at) \
                     VALUES (?1, NULL, ?2, ?3, ?4)",
                )
                .and_then(|mut stmt| {
                    stmt.execute(params![
                        tid,
                        EventKind::TaskStateChanged.as_str(),
                        payload.to_string(),
                        now
                    ])
                })
                .map_err(sql_err)?;
                cancelled_set.insert(tid.clone());
                cancelled_ids.push(tid);
            }

            let affected_dependents = collect_affected_dependents(tx, &cancelled_set)?;
            Ok(Ok(CancelOutcome {
                task_id: root_id,
                parent_id,
                cancelled_ids,
                affected_dependents,
            }))
        })
    }
}
