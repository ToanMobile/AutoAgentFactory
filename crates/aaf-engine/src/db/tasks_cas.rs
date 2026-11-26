//! Transactional task mutations on SQLite: atomic CAS state transitions,
//! single-transaction `Review -> Done` completion, and atomic task splitting.

use aaf_domain::blocked::BlockedReason;
use aaf_domain::budget::RootBudget;
use aaf_domain::dag;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{META_CONSECUTIVE_ROOT_FAILS, PortError, PortResult};
use aaf_domain::task::{Task, TaskState};
use rusqlite::{OptionalExtension, params};

use super::tasks::{
    EncodedTask, append_note_tx, encode_task, insert_task_row, require_task, task_not_found,
    to_json,
};
use super::{Db, corrupt, sql_err, to_i64};

/// Atomic compare-and-set state transition inside a single `BEGIN IMMEDIATE`
/// transaction. Returns `Ok(true)` when the task was in `expected` and moved to
/// `next`, or `Ok(false)` when its current state did not match `expected`.
pub(super) fn compare_and_set_state_impl(
    db: &Db,
    id: &str,
    expected: TaskState,
    next: TaskState,
    blocked: Option<&BlockedReason>,
    now: i64,
) -> PortResult<bool> {
    let id = id.to_string();
    let blocked = match (next, blocked) {
        (TaskState::Blocked, Some(reason)) => Some(to_json(reason, "blocked_reason")?),
        _ => None,
    };
    let reset_recover = matches!(
        next,
        TaskState::Done | TaskState::Review | TaskState::Cancelled
    );
    db.write(move |tx| {
        let current: Option<String> = tx
            .prepare_cached("SELECT state FROM tasks WHERE id = ?1")
            .and_then(|mut stmt| stmt.query_row(params![id], |row| row.get(0)).optional())
            .map_err(sql_err)?;
        let current = current.ok_or_else(|| task_not_found(&id))?;
        let current = TaskState::parse(&current).ok_or_else(|| corrupt("tasks.state", &current))?;
        if current != expected {
            return Ok(false);
        }
        if !current.can_transition_to(next) {
            return Err(PortError::Invalid(format!(
                "task {id}: transition {} -> {} is not allowed",
                current.as_str(),
                next.as_str()
            )));
        }
        let sql = if reset_recover {
            "UPDATE tasks SET state = ?1, blocked_reason = ?2, retry_after = NULL, \
             auto_recover_count = 0, updated_at = ?3 WHERE id = ?4 AND state = ?5"
        } else {
            "UPDATE tasks SET state = ?1, blocked_reason = ?2, retry_after = NULL, \
             updated_at = ?3 WHERE id = ?4 AND state = ?5"
        };
        let changed = tx
            .prepare_cached(sql)
            .and_then(|mut stmt| {
                stmt.execute(params![next.as_str(), blocked, now, id, expected.as_str()])
            })
            .map_err(sql_err)?;
        Ok(changed > 0)
    })
}

/// Completes a `Review` task to `Done` inside one `BEGIN IMMEDIATE` SQLite
/// transaction (CAS `Review -> Done` + `TaskStateChanged` + optional
/// `MergeResult` + `closed_fingerprints` + `consecutive_root_fails = 0` for
/// root tasks). Returns `Ok(true)` if transitioned, `Ok(false)` if not in `Review`.
pub fn complete_review_task(
    db: &Db,
    task_id: &str,
    why: &str,
    merge_payload: Option<&serde_json::Value>,
    now: i64,
) -> PortResult<bool> {
    db.complete_review_task(task_id, why, merge_payload, now)
}

impl Db {
    /// Atomically transitions a `Review` task to `Done` inside one
    /// `BEGIN IMMEDIATE` transaction: CAS `Review -> Done`, emits
    /// `TaskStateChanged` (and optional `MergeResult`), records
    /// `closed_fingerprints` if the task has a fingerprint, and resets
    /// `consecutive_root_fails` to `"0"` for root tasks.
    ///
    /// Returns `Ok(true)` if transitioned, `Ok(false)` if the task was no
    /// longer in `Review` (without writing any events or metadata).
    pub fn complete_review_task(
        &self,
        task_id: &str,
        why: &str,
        merge_payload: Option<&serde_json::Value>,
        now: i64,
    ) -> PortResult<bool> {
        let task_id = task_id.to_string();
        let state_payload =
            serde_json::json!({ "to": TaskState::Done.as_str(), "why": why }).to_string();
        let merge_payload = merge_payload.map(serde_json::Value::to_string);
        self.write(move |tx| {
            let row: Option<(String, String, String, Option<String>)> = tx
                .prepare_cached(
                    "SELECT state, root_id, repo_root, fingerprint FROM tasks WHERE id = ?1",
                )
                .and_then(|mut stmt| {
                    stmt.query_row(params![task_id], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                    })
                    .optional()
                })
                .map_err(sql_err)?;
            let (state_raw, root_id, repo_root, fingerprint) =
                row.ok_or_else(|| task_not_found(&task_id))?;
            let state =
                TaskState::parse(&state_raw).ok_or_else(|| corrupt("tasks.state", &state_raw))?;
            if state != TaskState::Review {
                return Ok(false);
            }
            let changed = tx
                .prepare_cached(
                    "UPDATE tasks SET state = ?1, blocked_reason = NULL, retry_after = NULL, \
                     auto_recover_count = 0, updated_at = ?2 WHERE id = ?3 AND state = ?4",
                )
                .and_then(|mut stmt| {
                    stmt.execute(params![
                        TaskState::Done.as_str(),
                        now,
                        task_id,
                        TaskState::Review.as_str(),
                    ])
                })
                .map_err(sql_err)?;
            if changed == 0 {
                return Ok(false);
            }
            tx.prepare_cached(
                "INSERT INTO events (task_id, run_id, kind, payload_json, created_at) \
                 VALUES (?1, NULL, ?2, ?3, ?4)",
            )
            .and_then(|mut stmt| {
                stmt.execute(params![
                    task_id,
                    EventKind::TaskStateChanged.as_str(),
                    state_payload,
                    now,
                ])
            })
            .map_err(sql_err)?;
            if let Some(payload) = merge_payload {
                tx.prepare_cached(
                    "INSERT INTO events (task_id, run_id, kind, payload_json, created_at) \
                     VALUES (?1, NULL, ?2, ?3, ?4)",
                )
                .and_then(|mut stmt| {
                    stmt.execute(params![
                        task_id,
                        EventKind::MergeResult.as_str(),
                        payload,
                        now,
                    ])
                })
                .map_err(sql_err)?;
            }
            if let Some(fp) = fingerprint {
                tx.prepare_cached(
                    "INSERT INTO closed_fingerprints (fingerprint, repo_root, task_id, closed_at) \
                     VALUES (?1, ?2, ?3, ?4)",
                )
                .and_then(|mut stmt| stmt.execute(params![fp, repo_root, task_id, now]))
                .map_err(sql_err)?;
            }
            if task_id == root_id {
                tx.prepare_cached(
                    "INSERT INTO meta (key, value) VALUES (?1, ?2) \
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                )
                .and_then(|mut stmt| stmt.execute(params![META_CONSECUTIVE_ROOT_FAILS, "0"]))
                .map_err(sql_err)?;
            }
            Ok(true)
        })
    }

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
        let encoded_children: Vec<EncodedTask> = children
            .iter()
            .map(encode_task)
            .collect::<PortResult<_>>()?;
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

            for enc in &encoded_children {
                insert_task_row(tx, enc, parent_state.as_str())?;
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

            append_note_tx(tx, &parent_id, parent_note, now)
        })
    }

    /// Atomically inserts `budget` only when no row exists for `budget.root_id`,
    /// then reads and returns the stored row inside the same write transaction.
    pub fn ensure_budget_if_missing_tx(&self, budget: &RootBudget) -> PortResult<RootBudget> {
        let budget = budget.clone();
        let tokens_used = to_i64(budget.tokens_used, "tokens_used")?;
        let token_limit = to_i64(budget.token_limit, "token_limit")?;
        let wall_secs_used = to_i64(budget.wall_secs_used, "wall_secs_used")?;
        let wall_secs_limit = to_i64(budget.wall_secs_limit, "wall_secs_limit")?;
        self.write(move |tx| {
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
            let mut stmt = tx
                .prepare_cached(
                    "SELECT tokens_used, token_limit, wall_secs_used, wall_secs_limit, \
                     cycles_used, cycles_limit, frozen FROM root_budgets WHERE root_id = ?1",
                )
                .map_err(sql_err)?;
            stmt.query_row(params![budget.root_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, u32>(4)?,
                    row.get::<_, u32>(5)?,
                    row.get::<_, bool>(6)?,
                ))
            })
            .map_err(sql_err)
            .and_then(|(tu, tl, wu, wl, cu, cl, frozen)| {
                Ok(RootBudget {
                    root_id: budget.root_id,
                    tokens_used: super::to_u64(tu, "root_budgets.tokens_used")?,
                    token_limit: super::to_u64(tl, "root_budgets.token_limit")?,
                    wall_secs_used: super::to_u64(wu, "root_budgets.wall_secs_used")?,
                    wall_secs_limit: super::to_u64(wl, "root_budgets.wall_secs_limit")?,
                    cycles_used: cu,
                    cycles_limit: cl,
                    frozen,
                })
            })
        })
    }

    /// Atomically synchronizes `token_limit`, `wall_secs_limit`, and `cycles_limit`
    /// from `cfg` into all non-frozen active root budgets of `repo_root` without
    /// touching `*_used` or `frozen`.
    pub fn sync_repo_budget_limits(
        &self,
        repo_root: &str,
        cfg: &aaf_domain::config::UnblockerConfig,
    ) -> PortResult<usize> {
        let token_limit = to_i64(cfg.max_token_budget, "token_limit")?;
        let wall_secs_limit = to_i64(cfg.max_wall_clock_secs, "wall_secs_limit")?;
        let cycles_limit = cfg.max_unblock_cycles;
        let repo_root = repo_root.to_string();
        self.write(move |tx| {
            tx.prepare_cached(
                "UPDATE root_budgets \
                 SET token_limit = ?1, wall_secs_limit = ?2, cycles_limit = ?3 \
                 WHERE frozen = 0 \
                   AND (token_limit != ?1 OR wall_secs_limit != ?2 OR cycles_limit != ?3) \
                   AND root_id IN ( \
                       SELECT DISTINCT root_id FROM tasks \
                       WHERE repo_root = ?4 \
                         AND state IN ('backlog', 'ready', 'running', 'gating', 'review') \
                   )",
            )
            .and_then(|mut stmt| {
                stmt.execute(params![
                    token_limit,
                    wall_secs_limit,
                    cycles_limit,
                    repo_root
                ])
            })
            .map_err(sql_err)
        })
    }
}
