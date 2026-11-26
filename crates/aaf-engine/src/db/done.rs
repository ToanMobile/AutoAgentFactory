//! Atomic `aaf board done` transaction (`BEGIN IMMEDIATE`) on SQLite.

use std::collections::{HashSet, VecDeque};

use aaf_domain::event::EventKind;
use aaf_domain::ports::{META_CONSECUTIVE_ROOT_FAILS, PortError, PortResult};
use aaf_domain::task::TaskState;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;

use super::{Db, corrupt, sql_err};

/// Options for [`Db::complete_task_manual_atomic`].
#[derive(Debug, Clone, Copy)]
pub struct DoneOptions<'a> {
    pub reason: &'a str,
    pub pr_number: Option<u64>,
    pub commit_sha: Option<&'a str>,
    pub force: bool,
}

/// Successful result of [`Db::complete_task_manual_atomic`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoneOutcome {
    pub task_id: String,
    pub previous_state: TaskState,
    pub parent_id: Option<String>,
    pub unblocked_dependents: Vec<String>,
}

/// Policy refusal returned by [`Db::complete_task_manual_atomic`] without modifying the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DoneRefusal {
    ActiveState { task_id: String, state: TaskState },
    AlreadyDone { task_id: String },
    CancelledRequiresForce { task_id: String },
    RunningRun { task_id: String, run_id: String },
    ActiveDescendant { child_id: String, state: TaskState },
    RunningDescendantRun { child_id: String, run_id: String },
    HasUnfinishedChildren { children: Vec<String> },
}

struct TaskDoneRow {
    state: String,
    root_id: String,
    repo_root: String,
    parent_id: Option<String>,
    fingerprint: Option<String>,
}

fn is_active_refusal(state: TaskState, allow_idle_gating: bool) -> bool {
    state == TaskState::Running || (!allow_idle_gating && state == TaskState::Gating)
}

fn first_running_run(conn: &Connection, task_id: &str) -> PortResult<Option<String>> {
    conn.prepare_cached("SELECT id FROM runs WHERE task_id = ?1 AND status = 'running' LIMIT 1")
        .and_then(|mut stmt| {
            stmt.query_row(params![task_id], |row| row.get(0))
                .optional()
        })
        .map_err(sql_err)
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

fn direct_non_terminal_dependents(conn: &Connection, task_id: &str) -> PortResult<Vec<String>> {
    let rows: Vec<(String, String)> = conn
        .prepare_cached(
            "SELECT d.task_id, t.state FROM task_deps d \
             JOIN tasks t ON t.id = d.task_id \
             WHERE d.blocked_by = ?1 ORDER BY d.task_id",
        )
        .and_then(|mut stmt| {
            stmt.query_map(params![task_id], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect()
        })
        .map_err(sql_err)?;
    let mut out = Vec::new();
    for (dep_id, raw_state) in rows {
        let state =
            TaskState::parse(&raw_state).ok_or_else(|| corrupt("tasks.state", &raw_state))?;
        if !state.is_terminal() {
            out.push(dep_id);
        }
    }
    Ok(out)
}

impl Db {
    /// Atomically marks a non-active task as `Done` (human action, e.g. after
    /// merging a PR outside `aaf merge`) inside one `BEGIN IMMEDIATE` transaction.
    ///
    /// Refuses active tasks (`Running`, or `Gating` unless `opts.force`), any
    /// task or descendant with a `running` run, already-`Done` tasks, `Cancelled`
    /// tasks unless `opts.force`, and tasks with unfinished subtasks unless
    /// `opts.force`.
    pub fn complete_task_manual_atomic(
        &self,
        id: &str,
        opts: &DoneOptions<'_>,
        now: i64,
    ) -> PortResult<Result<DoneOutcome, DoneRefusal>> {
        let task_id = id.to_string();
        let reason = opts.reason.to_string();
        let pr_number = opts.pr_number;
        let commit_sha = opts.commit_sha.map(str::to_string);
        let force = opts.force;

        self.write(move |tx| {
            let row: Option<TaskDoneRow> = tx
                .prepare_cached(
                    "SELECT state, root_id, repo_root, parent_id, fingerprint \
                     FROM tasks WHERE id = ?1",
                )
                .and_then(|mut stmt| {
                    stmt.query_row(params![task_id], |r| {
                        Ok(TaskDoneRow {
                            state: r.get(0)?,
                            root_id: r.get(1)?,
                            repo_root: r.get(2)?,
                            parent_id: r.get(3)?,
                            fingerprint: r.get(4)?,
                        })
                    })
                    .optional()
                })
                .map_err(sql_err)?;
            let Some(TaskDoneRow {
                state: raw_state,
                root_id,
                repo_root,
                parent_id,
                fingerprint,
            }) = row
            else {
                return Err(PortError::NotFound(format!("task {task_id}")));
            };
            let state =
                TaskState::parse(&raw_state).ok_or_else(|| corrupt("tasks.state", &raw_state))?;

            if state == TaskState::Done {
                return Ok(Err(DoneRefusal::AlreadyDone { task_id }));
            }
            if is_active_refusal(state, force) {
                return Ok(Err(DoneRefusal::ActiveState { task_id, state }));
            }
            if state == TaskState::Cancelled && !force {
                return Ok(Err(DoneRefusal::CancelledRequiresForce { task_id }));
            }
            if let Some(run_id) = first_running_run(tx, &task_id)? {
                return Ok(Err(DoneRefusal::RunningRun { task_id, run_id }));
            }

            let mut queue: VecDeque<String> = VecDeque::from([task_id.clone()]);
            let mut visited: HashSet<String> = HashSet::from([task_id.clone()]);
            let mut unfinished_children: Vec<String> = Vec::new();
            while let Some(parent) = queue.pop_front() {
                for (child_id, child_state) in direct_children(tx, &parent)? {
                    if !visited.insert(child_id.clone()) {
                        continue;
                    }
                    if is_active_refusal(child_state, force) {
                        return Ok(Err(DoneRefusal::ActiveDescendant {
                            child_id,
                            state: child_state,
                        }));
                    }
                    if let Some(run_id) = first_running_run(tx, &child_id)? {
                        return Ok(Err(DoneRefusal::RunningDescendantRun { child_id, run_id }));
                    }
                    if !child_state.is_terminal() {
                        unfinished_children.push(child_id.clone());
                    }
                    queue.push_back(child_id);
                }
            }
            if !unfinished_children.is_empty() && !force {
                return Ok(Err(DoneRefusal::HasUnfinishedChildren {
                    children: unfinished_children,
                }));
            }

            let changed = tx
                .prepare_cached(
                    "UPDATE tasks SET state = 'done', blocked_reason = NULL, \
                     retry_after = NULL, auto_recover_count = 0, updated_at = ?1 \
                     WHERE id = ?2 AND state = ?3",
                )
                .and_then(|mut stmt| stmt.execute(params![now, task_id, state.as_str()]))
                .map_err(sql_err)?;
            if changed != 1 {
                return Err(PortError::Conflict(format!(
                    "task {task_id} changed state concurrently during done"
                )));
            }

            let mut payload = json!({
                "from": state.as_str(),
                "to": TaskState::Done.as_str(),
                "why": reason,
                "reason": reason,
                "by": "human",
                "source": "aaf_board_done",
            });
            if let Some(obj) = payload.as_object_mut() {
                if let Some(pr) = pr_number {
                    obj.insert("pr_number".to_string(), json!(pr));
                }
                if let Some(sha) = commit_sha.as_deref() {
                    obj.insert("commit_sha".to_string(), json!(sha));
                }
            }
            tx.prepare_cached(
                "INSERT INTO events (task_id, run_id, kind, payload_json, created_at) \
                 VALUES (?1, NULL, ?2, ?3, ?4)",
            )
            .and_then(|mut stmt| {
                stmt.execute(params![
                    task_id,
                    EventKind::TaskStateChanged.as_str(),
                    payload.to_string(),
                    now,
                ])
            })
            .map_err(sql_err)?;

            if pr_number.is_some() || commit_sha.is_some() {
                let mut merge_ev = json!({
                    "merged": true,
                    "kind": "manual_done",
                    "by": "human",
                });
                if let Some(obj) = merge_ev.as_object_mut() {
                    if let Some(pr) = pr_number {
                        obj.insert("pr_number".to_string(), json!(pr));
                    }
                    if let Some(sha) = commit_sha.as_deref() {
                        obj.insert("head_sha".to_string(), json!(sha));
                    }
                }
                tx.prepare_cached(
                    "INSERT INTO events (task_id, run_id, kind, payload_json, created_at) \
                     VALUES (?1, NULL, ?2, ?3, ?4)",
                )
                .and_then(|mut stmt| {
                    stmt.execute(params![
                        task_id,
                        EventKind::MergeResult.as_str(),
                        merge_ev.to_string(),
                        now,
                    ])
                })
                .map_err(sql_err)?;
            }

            if let Some(fp) = fingerprint {
                let exists: bool = tx
                    .prepare_cached(
                        "SELECT 1 FROM closed_fingerprints \
                         WHERE fingerprint = ?1 AND repo_root = ?2 AND task_id = ?3 LIMIT 1",
                    )
                    .and_then(|mut stmt| {
                        stmt.query_row(params![fp, repo_root, task_id], |_| Ok(true))
                            .optional()
                    })
                    .map_err(sql_err)?
                    .unwrap_or(false);
                if !exists {
                    tx.prepare_cached(
                        "INSERT INTO closed_fingerprints \
                         (fingerprint, repo_root, task_id, closed_at) \
                         VALUES (?1, ?2, ?3, ?4)",
                    )
                    .and_then(|mut stmt| stmt.execute(params![fp, repo_root, task_id, now]))
                    .map_err(sql_err)?;
                }
            }

            if task_id == root_id {
                tx.prepare_cached(
                    "INSERT INTO meta (key, value) VALUES (?1, ?2) \
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                )
                .and_then(|mut stmt| stmt.execute(params![META_CONSECUTIVE_ROOT_FAILS, "0"]))
                .map_err(sql_err)?;
            }

            let unblocked_dependents = direct_non_terminal_dependents(tx, &task_id)?;
            Ok(Ok(DoneOutcome {
                task_id,
                previous_state: state,
                parent_id,
                unblocked_dependents,
            }))
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use aaf_domain::event::EventKind;
    use aaf_domain::ports::{EventSink, RunStore, TaskStore};
    use aaf_domain::run::{RunRecord, RunStatus};
    use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};

    use super::*;

    fn mk_task(id: &str, state: TaskState, parent: Option<&str>) -> Task {
        let spec = ImmutableSpec {
            description: "spec".into(),
            acceptance: vec!["ok".into()],
        };
        let mut t = Task::new_root(id, "/repo/a", id, spec, Priority::P2, 10);
        if let Some(p) = parent {
            t.parent_id = Some(p.into());
            t.root_id = p.into();
            t.depth = 1;
        }
        t.state = state;
        t
    }

    #[test]
    fn marks_review_done_records_merge_result_and_reports_dependents() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
        let mut t1 = mk_task("t-1", TaskState::Review, None);
        t1.fingerprint = Some("fp-1".into());
        let t2 = mk_task("t-2", TaskState::Ready, None);
        db.insert_task(&t1).unwrap();
        db.insert_task(&t2).unwrap();
        db.add_dep("t-2", "t-1").unwrap();

        let opts = DoneOptions {
            reason: "Merged PR #455 into main",
            pr_number: Some(455),
            commit_sha: Some("bcbd084e"),
            force: false,
        };
        let out = db
            .complete_task_manual_atomic("t-1", &opts, 200)
            .unwrap()
            .unwrap();
        assert_eq!(out.previous_state, TaskState::Review);
        assert_eq!(out.unblocked_dependents, vec!["t-2".to_string()]);
        assert_eq!(db.get_task("t-1").unwrap().state, TaskState::Done);

        let evs = db
            .events_for_task_by_kinds("t-1", &[EventKind::TaskStateChanged, EventKind::MergeResult], 10)
            .unwrap();
        assert_eq!(evs.len(), 2);
    }

    #[test]
    fn refuses_cancelled_without_force_and_allows_with_force() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
        db.insert_task(&mk_task("t-c", TaskState::Cancelled, None))
            .unwrap();

        let no_force = DoneOptions {
            reason: "PR merged",
            pr_number: Some(455),
            commit_sha: None,
            force: false,
        };
        assert_eq!(
            db.complete_task_manual_atomic("t-c", &no_force, 200)
                .unwrap(),
            Err(DoneRefusal::CancelledRequiresForce {
                task_id: "t-c".into()
            })
        );

        let forced = DoneOptions {
            force: true,
            ..no_force
        };
        let out = db
            .complete_task_manual_atomic("t-c", &forced, 201)
            .unwrap()
            .unwrap();
        assert_eq!(out.previous_state, TaskState::Cancelled);
        assert_eq!(db.get_task("t-c").unwrap().state, TaskState::Done);
    }

    #[test]
    fn refuses_active_task_running_run_and_unfinished_children() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
        db.insert_task(&mk_task("t-run", TaskState::Running, None))
            .unwrap();
        db.insert_task(&mk_task("t-gate", TaskState::Gating, None))
            .unwrap();
        db.insert_task(&mk_task("t-p", TaskState::Review, None))
            .unwrap();
        db.insert_task(&mk_task("t-ch", TaskState::Ready, Some("t-p")))
            .unwrap();

        let opts = DoneOptions {
            reason: "merged",
            pr_number: None,
            commit_sha: None,
            force: false,
        };
        assert!(matches!(
            db.complete_task_manual_atomic("t-run", &opts, 200).unwrap(),
            Err(DoneRefusal::ActiveState {
                state: TaskState::Running,
                ..
            })
        ));
        assert!(matches!(
            db.complete_task_manual_atomic("t-gate", &opts, 200).unwrap(),
            Err(DoneRefusal::ActiveState {
                state: TaskState::Gating,
                ..
            })
        ));
        assert_eq!(
            db.complete_task_manual_atomic("t-p", &opts, 200).unwrap(),
            Err(DoneRefusal::HasUnfinishedChildren {
                children: vec!["t-ch".into()]
            })
        );

        let run = RunRecord {
            id: "r-live".into(),
            task_id: "t-gate".into(),
            account_id: None,
            executor: "codex".into(),
            worktree_path: "/tmp/wt".into(),
            branch: "aaf/t-gate".into(),
            status: RunStatus::Running,
            exit_code: None,
            tokens_in: 0,
            tokens_out: 0,
            started_at: 100,
            ended_at: None,
        };
        db.insert_run(&run).unwrap();
        let forced = DoneOptions {
            force: true,
            ..opts
        };
        assert_eq!(
            db.complete_task_manual_atomic("t-gate", &forced, 200)
                .unwrap(),
            Err(DoneRefusal::RunningRun {
                task_id: "t-gate".into(),
                run_id: "r-live".into()
            })
        );
    }
}
