//! Safe reaping of stale `Running` runs and zombie `Running` tasks on idle
//! repositories during `aaf gc --reap-stale`.
//!
//! Callers MUST hold the repository's `.run.lock` set before invoking
//! [`reap_repo_stale_runs`] so no active `aaf run` process can be disturbed.

use std::collections::{BTreeSet, HashSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{EventSink, RunStore, TaskStore};
use aaf_domain::run::{RunFinish, RunStatus};
use aaf_domain::task::{Task, TaskState};
use serde_json::json;

use super::task_clean::canonical_main_repo;
use super::{GcError, GcOptions, GcReport, collect_db_state, try_lock_repo_set};
use crate::db::Db;

/// Reaps stale runs for an already-locked repo when `opts.reap_stale` is enabled,
/// removing reaped run IDs from `running_runs`.
pub(super) fn reap_repo_if_enabled(
    repo: &Path,
    data_dir: &Path,
    fresh_tasks: &[Task],
    opts: &GcOptions,
    now: SystemTime,
    running_runs: &mut HashSet<String>,
    report: &mut GcReport,
) {
    if !opts.reap_stale {
        return;
    }
    for id in reap_repo_stale_runs(repo, data_dir, fresh_tasks, opts.dry_run, now, report) {
        running_runs.remove(&id);
    }
}

/// Acquires `.run.lock` for a missing/non-git repo and reaps its stale DB runs
/// when `opts.reap_stale` is enabled.
pub(super) fn reap_missing_repo_stale_runs(
    repo: &Path,
    data_dir: &Path,
    lock_ids: &BTreeSet<String>,
    opts: &GcOptions,
    now: SystemTime,
    running_runs: &mut HashSet<String>,
    report: &mut GcReport,
) -> Result<(), GcError> {
    if !opts.reap_stale {
        return Ok(());
    }
    if let Some(_locks) = try_lock_repo_set(data_dir, lock_ids, opts.dry_run)?
        && let Ok((fresh_tasks, _, _)) = collect_db_state(data_dir)
    {
        for id in reap_repo_stale_runs(repo, data_dir, &fresh_tasks, opts.dry_run, now, report) {
            running_runs.remove(&id);
        }
    }
    Ok(())
}

/// Reaps `RunStatus::Running` runs (marking them `Killed`) and returns
/// `TaskState::Running` tasks to `Ready` for `repo` (caller holds `.run.lock`).
///
/// Respects `dry_run`: scans read-only and reports what would be reaped without
/// opening the DB read-write. Returns the IDs of runs that were reaped.
pub(super) fn reap_repo_stale_runs(
    repo: &Path,
    data_dir: &Path,
    all_tasks: &[Task],
    dry_run: bool,
    now: SystemTime,
    report: &mut GcReport,
) -> HashSet<String> {
    let db_path = data_dir.join("aaf.db");
    if !db_path.is_file() {
        return HashSet::new();
    }
    let repo_tasks: Vec<&Task> = all_tasks
        .iter()
        .filter(|t| {
            !t.repo_root.trim().is_empty() && canonical_main_repo(Path::new(&t.repo_root)) == repo
        })
        .collect();
    if repo_tasks.is_empty() {
        return HashSet::new();
    }

    let ro_db = match Db::open_read_only(&db_path) {
        Ok(db) => db,
        Err(err) => {
            report
                .warnings
                .push(format!("{}: stale-run scan failed: {err}", repo.display()));
            return HashSet::new();
        }
    };

    let mut stale_runs = Vec::new();
    let mut stale_tasks = Vec::new();
    for task in &repo_tasks {
        if let Ok(runs) = ro_db.runs_for_task(&task.id) {
            for run in runs {
                if run.status == RunStatus::Running {
                    stale_runs.push((task.id.clone(), run));
                }
            }
        }
        if task.state == TaskState::Running {
            stale_tasks.push(task.id.clone());
        }
    }
    drop(ro_db);

    if stale_runs.is_empty() && stale_tasks.is_empty() {
        return HashSet::new();
    }

    if dry_run {
        let mut ids = HashSet::new();
        for (_, run) in stale_runs {
            ids.insert(run.id.clone());
            report.reaped_stale_runs.push(run.id);
        }
        report.recovered_running_tasks.extend(stale_tasks);
        return ids;
    }

    let db = match Db::open(&db_path) {
        Ok(db) => db,
        Err(err) => {
            report.warnings.push(format!(
                "{}: stale-run reap open failed: {err}",
                repo.display()
            ));
            return HashSet::new();
        }
    };
    let now_secs = now
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0);

    let mut reaped_ids = HashSet::new();
    for (task_id, run) in stale_runs {
        let still_running = db
            .runs_for_task(&task_id)
            .ok()
            .and_then(|runs| runs.into_iter().find(|r| r.id == run.id))
            .is_some_and(|r| r.status == RunStatus::Running);
        if !still_running {
            continue;
        }
        let finish = RunFinish {
            status: RunStatus::Killed,
            exit_code: None,
            tokens_in: run.tokens_in,
            tokens_out: run.tokens_out,
            ended_at: now_secs,
        };
        if let Err(err) = db.finish_run(&run.id, &finish) {
            report.warnings.push(format!("reap run {}: {err}", run.id));
            continue;
        }
        let payload = json!({
            "run_id": run.id,
            "status": "killed",
            "tokens_in": run.tokens_in,
            "tokens_out": run.tokens_out,
            "reaped_by": "aaf gc --reap-stale",
        });
        let _ = db.emit(
            &FactoryEvent::new(EventKind::RunFinished, payload.to_string(), now_secs)
                .for_task(&task_id),
        );
        reaped_ids.insert(run.id.clone());
        report.reaped_stale_runs.push(run.id);
    }

    for task_id in stale_tasks {
        let still_running = db
            .get_task(&task_id)
            .is_ok_and(|t| t.state == TaskState::Running);
        if !still_running {
            continue;
        }
        if let Err(err) = db.set_state(&task_id, TaskState::Ready, None, now_secs) {
            report
                .warnings
                .push(format!("recover running task {task_id}: {err}"));
            continue;
        }
        let payload = json!({
            "to": "ready",
            "why": "reaped stale running task by aaf gc --reap-stale",
        });
        let _ = db.emit(
            &FactoryEvent::new(EventKind::TaskStateChanged, payload.to_string(), now_secs)
                .for_task(&task_id),
        );
        report.recovered_running_tasks.push(task_id);
    }

    reaped_ids
}
