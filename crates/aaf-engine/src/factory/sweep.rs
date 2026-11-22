//! Recovery, review sweeps, live gating polls, and split-parent completion for
//! [`super::scheduler::Scheduler`].

use std::collections::{HashMap, HashSet};

use aaf_domain::event::EventKind;
use aaf_domain::ports::{PortError, PortResult};
use aaf_domain::run::{RunFinish, RunStatus};
use aaf_domain::task::{Task, TaskState};
use serde_json::json;

use super::scheduler::Scheduler;
use super::{Deps, finish, unblock};
use crate::git::{GitError, TaskWorktree};

fn reap_running_runs(
    deps: &Deps,
    task_id: &str,
    reap: &dyn Fn(&str) -> PortResult<()>,
) -> PortResult<()> {
    for run in deps.store.runs_for_task(task_id)? {
        if run.status != RunStatus::Running {
            continue;
        }
        reap(&run.id)?;
        deps.store.finish_run(
            &run.id,
            &RunFinish {
                status: RunStatus::Killed,
                exit_code: None,
                tokens_in: run.tokens_in,
                tokens_out: run.tokens_out,
                ended_at: deps.now(),
            },
        )?;
    }
    Ok(())
}

/// Puts back what an interrupted `aaf run` left behind: a task still
/// `Running` has no live run any more (its processes are reaped and it
/// returns to Ready); a task still `Gating` has any interrupted audit/unblock
/// runs reaped and only needs the gate again.
pub fn recover(deps: &Deps, reap: &dyn Fn(&str) -> PortResult<()>) -> PortResult<Vec<String>> {
    let repo = Some(deps.settings.repo_root.as_str());
    for task in deps.store.list_tasks(repo, Some(TaskState::Running))? {
        reap_running_runs(deps, &task.id, reap)?;
        deps.move_to(
            &task.id,
            TaskState::Ready,
            None,
            "left running by an interrupted run",
        )?;
    }
    let gating = deps.store.list_tasks(repo, Some(TaskState::Gating))?;
    for task in &gating {
        reap_running_runs(deps, &task.id, reap)?;
    }
    Ok(gating.into_iter().map(|task| task.id).collect())
}

/// Cleans orphaned `aaf/batch-*` integration worktrees/branches and prunes
/// stale git worktree entries at scheduler startup. Non-fatal warnings are
/// returned rather than aborting startup.
pub(super) fn startup_sweep(deps: &Deps) -> Vec<String> {
    let mut warnings = Vec::new();
    let repo = Some(deps.settings.repo_root.as_str());
    let active_ids: HashSet<String> = match deps.store.list_tasks(repo, None) {
        Ok(tasks) => tasks.into_iter().map(|task| task.id).collect(),
        Err(err) => {
            warnings.push(format!("startup_sweep list_tasks: {err}"));
            return warnings;
        }
    };
    if let Err(err) = deps.git.sweep_orphan_batches(&active_ids) {
        warnings.push(format!("startup_sweep orphan batches: {err}"));
    }
    if let Err(err) = deps.git.prune_worktrees() {
        warnings.push(format!("startup_sweep prune worktrees: {err}"));
    }
    warnings
}

impl<'a> Scheduler<'a> {
    /// Picks up live `Gating` tasks (e.g. `aaf board rework` without `--rerun`)
    /// in `Mode::Auto` without restarting the daemon.
    pub(super) fn poll_gating(&mut self) -> PortResult<()> {
        if self.default_cooling_down() {
            return Ok(());
        }
        let repo = Some(self.deps.settings.repo_root.as_str());
        for task in self.deps.store.list_tasks(repo, Some(TaskState::Gating))? {
            if self.gating_in_flight.contains(&task.id) || self.parked.contains(&task.id) {
                continue;
            }
            self.queue_gating_task(task.id)?;
        }
        Ok(())
    }

    /// A task in Review whose branch has reached the base is done: this is
    /// how a merged pull request releases the tasks that depend on it.
    pub(super) fn sweep_review(&mut self, base_is_fresh: bool) -> PortResult<()> {
        let deps = self.deps;
        let repo = Some(deps.settings.repo_root.as_str());
        let review = deps.store.list_tasks(repo, Some(TaskState::Review))?;
        // Offline: the tasks stay in Review and are looked at again on the
        // next sweep.
        if review.is_empty() || !(base_is_fresh || self.fetch_base()) {
            return Ok(());
        }
        let mut remaining: Vec<Task> = Vec::new();
        for task in review {
            if !deps.git.is_merged(&task.id)? {
                remaining.push(task);
                continue;
            }
            if deps.store.get_task(&task.id)?.state != TaskState::Review {
                continue;
            }
            match deps.move_to(
                &task.id,
                TaskState::Done,
                None,
                "branch is merged into the base",
            ) {
                Ok(()) => {}
                // A concurrent `aaf board rework` moved the task out of Review.
                Err(PortError::Invalid(_)) => continue,
                Err(err) => return Err(err),
            }
            finish::close(deps, &task)?;
            unblock::root_succeeded(deps, &task)?;
            if let Err(err) = deps.git.cleanup(&task.id) {
                self.summary.errors.push(format!("{}: {err}", task.id));
            }
            self.summary.done.push(task.id);
        }
        self.sweep_squash_merged(remaining)
    }

    fn sweep_squash_merged(&mut self, remaining: Vec<Task>) -> PortResult<()> {
        if remaining.is_empty() {
            return Ok(());
        }
        let deps = self.deps;
        let mut candidates: Vec<(String, String)> = Vec::new();
        let mut worktrees: HashMap<String, (Task, TaskWorktree)> = HashMap::new();
        for task in remaining {
            match deps.git.existing_worktree_head(&task.id) {
                Ok(Some((wt, head_oid))) => {
                    candidates.push((task.id.clone(), head_oid));
                    worktrees.insert(task.id.clone(), (task, wt));
                }
                Ok(None) => {}
                Err(err) => self.summary.errors.push(format!("{}: {err}", task.id)),
            }
        }
        if candidates.is_empty() {
            return Ok(());
        }
        let verified = match deps.git.verified_squash_merges(&candidates) {
            Ok(map) => map,
            Err(err) => {
                self.summary.errors.push(format!("squash_sweep: {err}"));
                return Ok(());
            }
        };
        for (task_id, _) in &candidates {
            let Some(expected_oid) = verified.get(task_id) else {
                continue;
            };
            let Some((task, wt)) = worktrees.remove(task_id) else {
                continue;
            };
            if deps.store.get_task(&task.id)?.state != TaskState::Review {
                continue;
            }
            match deps.git.remove_verified_merged(&wt, expected_oid) {
                Ok(()) => {}
                Err(GitError::Dirty(path)) => {
                    self.summary.errors.push(format!(
                        "{}: skipped squash-merge cleanup: uncommitted changes in {}",
                        task.id,
                        path.display()
                    ));
                    continue;
                }
                Err(err) => {
                    self.summary
                        .errors
                        .push(format!("{}: squash-merge cleanup failed: {err}", task.id));
                    continue;
                }
            }
            match deps.move_to(
                &task.id,
                TaskState::Done,
                None,
                "pull request squash-merged into the base",
            ) {
                Ok(()) => {}
                Err(PortError::Invalid(_)) => continue,
                Err(err) => return Err(err),
            }
            deps.emit(
                EventKind::MergeResult,
                &task.id,
                json!({
                    "merged": true,
                    "kind": "squash_pr",
                    "head_oid": expected_oid,
                }),
            )?;
            finish::close(deps, &task)?;
            unblock::root_succeeded(deps, &task)?;
            self.summary.done.push(task.id);
        }
        Ok(())
    }

    /// False when the remote could not be reached; the run goes on with the
    /// base it has.
    pub(super) fn fetch_base(&mut self) -> bool {
        match self.deps.git.fetch_base() {
            Ok(()) => true,
            Err(err) => {
                self.summary.errors.push(format!("fetch: {err}"));
                false
            }
        }
    }

    /// Auto-completes a proactive split container root whose subtasks are all
    /// `Done`, so the scheduler never spawns a redundant agent that fails with
    /// `NO_CHANGE`.
    pub(super) fn try_auto_complete_split_parent(&mut self, task: &Task) -> PortResult<bool> {
        if task.depth != 0
            || !task
                .context_notes
                .iter()
                .any(|n| n.starts_with("split: spawned "))
        {
            return Ok(false);
        }
        let children = self.deps.store.children_of(&task.id)?;
        if children.len() < 2 || !children.iter().all(|c| c.state == TaskState::Done) {
            return Ok(false);
        }
        self.deps.move_to(
            &task.id,
            TaskState::Running,
            None,
            "all split subtasks completed",
        )?;
        self.deps.move_to(
            &task.id,
            TaskState::Gating,
            None,
            "all split subtasks completed",
        )?;
        self.deps.move_to(
            &task.id,
            TaskState::Done,
            None,
            "all split subtasks completed",
        )?;
        finish::close(self.deps, task)?;
        unblock::root_succeeded(self.deps, task)?;
        self.summary.done.push(task.id.clone());
        Ok(true)
    }
}
