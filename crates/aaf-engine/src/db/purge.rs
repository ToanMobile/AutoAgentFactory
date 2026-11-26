//! Atomic soft-archival of terminal (`done` / `cancelled`) tasks from the Board UI
//! (`BEGIN IMMEDIATE`), preserving `runs`, `events`, `task_deps`, `cost_guard`,
//! `aaf gc`, and `/studio` telemetry intact.

use std::collections::HashSet;

use aaf_domain::ports::PortResult;
use aaf_domain::task::TaskState;
use rusqlite::{Connection, OptionalExtension, params};

use super::{Db, corrupt, sql_err};

/// Result of [`Db::archive_terminal_tasks_atomic`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PurgeOutcome {
    pub deleted_ids: Vec<String>,
    pub skipped_ids: Vec<String>,
}

struct CandidateRow {
    state: TaskState,
    repo_root: String,
    archived_at: Option<i64>,
}

fn load_candidate(conn: &Connection, id: &str) -> PortResult<Option<CandidateRow>> {
    let row: Option<(String, String, Option<i64>)> = conn
        .prepare_cached("SELECT state, repo_root, archived_at FROM tasks WHERE id = ?1")
        .and_then(|mut stmt| {
            stmt.query_row(params![id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .optional()
        })
        .map_err(sql_err)?;
    match row {
        None => Ok(None),
        Some((raw_state, repo_root, archived_at)) => {
            let state =
                TaskState::parse(&raw_state).ok_or_else(|| corrupt("tasks.state", &raw_state))?;
            Ok(Some(CandidateRow {
                state,
                repo_root,
                archived_at,
            }))
        }
    }
}

fn has_running_run(conn: &Connection, task_id: &str) -> PortResult<bool> {
    conn.prepare_cached("SELECT 1 FROM runs WHERE task_id = ?1 AND status = 'running' LIMIT 1")
        .and_then(|mut stmt| stmt.exists(params![task_id]))
        .map_err(sql_err)
}

impl Db {
    /// Atomically marks terminal (`done` or `cancelled`) tasks listed in `task_ids`
    /// as archived (`archived_at = now`), scoped to `repo_scope` when `Some`.
    ///
    /// Non-terminal tasks, out-of-scope tasks, and tasks with active running runs
    /// are skipped and reported in [`PurgeOutcome::skipped_ids`]. Already-archived
    /// or missing IDs are ignored idempotently. No `tasks`, `runs`, `events`,
    /// `task_deps`, or `root_budgets` rows are deleted, preserving `cost_guard`,
    /// account routing, split-parent invariants, `aaf gc`, and `/studio` metrics.
    pub fn archive_terminal_tasks_atomic(
        &self,
        task_ids: &[String],
        repo_scope: Option<&str>,
        now: i64,
    ) -> PortResult<PurgeOutcome> {
        let mut unique_ids: Vec<String> = Vec::with_capacity(task_ids.len());
        let mut seen: HashSet<String> = HashSet::with_capacity(task_ids.len());
        for raw in task_ids {
            let id = raw.trim();
            if !id.is_empty() && seen.insert(id.to_string()) {
                unique_ids.push(id.to_string());
            }
        }
        if unique_ids.is_empty() {
            return Ok(PurgeOutcome::default());
        }

        let scope = repo_scope.map(str::to_string);
        self.write(move |tx| {
            let mut deleted_ids: Vec<String> = Vec::new();
            let mut skipped_ids: Vec<String> = Vec::new();

            for id in &unique_ids {
                let Some(row) = load_candidate(tx, id)? else {
                    continue;
                };
                if row.archived_at.is_some() {
                    continue;
                }
                if scope.as_deref().is_some_and(|s| row.repo_root != s) {
                    skipped_ids.push(id.clone());
                    continue;
                }
                if !matches!(row.state, TaskState::Done | TaskState::Cancelled)
                    || has_running_run(tx, id)?
                {
                    skipped_ids.push(id.clone());
                    continue;
                }
                let updated = tx
                    .prepare_cached(
                        "UPDATE tasks SET archived_at = ?1 \
                         WHERE id = ?2 AND state IN ('done', 'cancelled') AND archived_at IS NULL",
                    )
                    .and_then(|mut s| s.execute(params![now, id]))
                    .map_err(sql_err)?;
                if updated > 0 {
                    deleted_ids.push(id.clone());
                }
            }

            deleted_ids.sort();
            skipped_ids.sort();
            Ok(PurgeOutcome {
                deleted_ids,
                skipped_ids,
            })
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use aaf_domain::budget::RootBudget;
    use aaf_domain::config::UnblockerConfig;
    use aaf_domain::event::{EventKind, FactoryEvent};
    use aaf_domain::ports::{BudgetStore, EventSink, RunStore, TaskStore};
    use aaf_domain::run::{RunRecord, RunStatus};
    use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};

    use super::*;

    fn mk_task(id: &str, repo: &str, state: TaskState, parent: Option<&str>) -> Task {
        let mut t = Task::new_root(
            id,
            repo,
            format!("Task {id}"),
            ImmutableSpec {
                description: "spec".into(),
                acceptance: vec!["ac".into()],
            },
            Priority::P2,
            100,
        );
        t.state = state;
        if let Some(p) = parent {
            t.parent_id = Some(p.to_string());
            t.root_id = p.to_string();
            t.depth = 1;
        }
        t
    }

    fn mk_run(id: &str, task_id: &str, status: RunStatus) -> RunRecord {
        RunRecord {
            id: id.into(),
            task_id: task_id.into(),
            account_id: Some("acct-1".into()),
            executor: "claude".into(),
            worktree_path: "/tmp/wt".into(),
            branch: format!("aaf/{task_id}"),
            status,
            exit_code: Some(0),
            tokens_in: 1200,
            tokens_out: 300,
            started_at: 100,
            ended_at: (status != RunStatus::Running).then_some(110),
        }
    }

    #[test]
    fn archives_terminal_tasks_from_board_while_preserving_runs_events_and_cost_accounting() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
        let cfg = UnblockerConfig::default();
        let done = mk_task("t-done", "/repo/a", TaskState::Done, None);
        let canc = mk_task("t-canc", "/repo/a", TaskState::Cancelled, None);
        let ready = mk_task("t-ready", "/repo/a", TaskState::Ready, None);
        let other_repo = mk_task("t-other", "/repo/b", TaskState::Done, None);
        for t in [&done, &canc, &ready, &other_repo] {
            db.insert_task(t).unwrap();
            db.put_budget(&RootBudget::new(&t.root_id, &cfg)).unwrap();
        }
        db.insert_run(&mk_run("r-done", "t-done", RunStatus::Succeeded))
            .unwrap();
        db.emit(&FactoryEvent::new(EventKind::TaskStateChanged, "{}", 101).for_task("t-done"))
            .unwrap();

        let ids: Vec<String> = vec![
            "t-done".into(),
            "t-canc".into(),
            "t-ready".into(),
            "t-other".into(),
            "t-missing".into(),
        ];
        let out = db
            .archive_terminal_tasks_atomic(&ids, Some("/repo/a"), 500)
            .unwrap();
        assert_eq!(out.deleted_ids, vec!["t-canc", "t-done"]);
        assert_eq!(out.skipped_ids, vec!["t-other", "t-ready"]);

        // Hidden from Board UI, visible to engine/GC/Studio.
        let board_a: Vec<String> = db
            .list_board_tasks(Some("/repo/a"), None)
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(board_a, vec!["t-ready"]);
        let all_a = db.list_tasks(Some("/repo/a"), None).unwrap();
        assert_eq!(all_a.len(), 3);

        // Runs, events, budgets, and cost window usage stay 100% intact.
        assert_eq!(db.runs_for_task("t-done").unwrap().len(), 1);
        assert_eq!(db.events_for_task("t-done", 10).unwrap().len(), 1);
        assert!(db.get_budget("t-done").is_ok());
        let (total_tokens, earliest_ts) = db.cost_window_usage(0).unwrap();
        assert_eq!((total_tokens, earliest_ts), (1500, Some(110)));

        // Idempotent second call archives nothing.
        let again = db
            .archive_terminal_tasks_atomic(&["t-done".into()], Some("/repo/a"), 501)
            .unwrap();
        assert!(again.deleted_ids.is_empty());
        assert!(again.skipped_ids.is_empty());
    }

    #[test]
    fn archiving_subtask_keeps_split_parent_children_and_deps_intact() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
        let parent = mk_task("t-p", "/repo/a", TaskState::Ready, None);
        let child = mk_task("t-c", "/repo/a", TaskState::Done, Some("t-p"));
        db.insert_task(&parent).unwrap();
        db.insert_task(&child).unwrap();
        db.add_dep("t-p", "t-c").unwrap();

        let out = db
            .archive_terminal_tasks_atomic(&["t-c".into()], None, 500)
            .unwrap();
        assert_eq!(out.deleted_ids, vec!["t-c"]);
        assert!(out.skipped_ids.is_empty());
        // Engine still sees child and dep for split-parent auto-completion.
        assert_eq!(db.children_of("t-p").unwrap().len(), 1);
        assert_eq!(db.deps_of("t-p").unwrap(), vec!["t-c"]);
        // Board hides the archived child cleanly.
        let board_ids: Vec<String> = db
            .list_board_tasks(Some("/repo/a"), None)
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(board_ids, vec!["t-p"]);
    }
}
