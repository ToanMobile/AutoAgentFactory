//! Review and squash-merge sweeps for [`super::scheduler::Scheduler`].
//!
//! Uses atomic CAS (`complete_review_task`: `Review -> Done`) before touching
//! any worktree on disk so a concurrent `aaf board rework` (`Review -> Gating`)
//! can never have its worktree deleted or state overwritten.

use std::collections::HashMap;

use aaf_domain::ports::PortResult;
use aaf_domain::task::{Task, TaskState};
use serde_json::json;

use super::scheduler::Scheduler;
use crate::git::{GitError, TaskWorktree};

impl Scheduler<'_> {
    /// A task in Review whose branch has reached the base is done: this is
    /// how a merged pull request releases the tasks that depend on it.
    pub(super) fn sweep_review(&mut self, base_is_fresh: bool) -> PortResult<()> {
        let deps = self.deps;
        let repo = Some(deps.settings.repo_root.as_str());
        let review = deps.store.list_tasks(repo, Some(TaskState::Review))?;
        if review.is_empty() {
            return Ok(());
        }
        self.state.last_review_sweep = Some(std::time::Instant::now());
        // Offline: the tasks stay in Review and are looked at again on the
        // next sweep.
        if !(base_is_fresh || self.fetch_base()) {
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
            if !deps.store.complete_review_task(
                &task.id,
                "branch is merged into the base",
                None,
                deps.now(),
            )? {
                continue;
            }
            if let Err(err) = deps.git.cleanup(&task.id) {
                self.summary
                    .errors
                    .push(format!("{}: leftover: {err}", task.id));
            }
            self.clear_restarts_for_task(&task.id);
            self.summary.done.push(task.id);
        }
        self.sweep_squash_merged(remaining)?;
        self.sweep_merge_retry()?;
        self.sweep_pr_feedback()
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
            let merge_payload = json!({
                "merged": true,
                "kind": "squash_pr",
                "head_oid": expected_oid,
                "head_sha": expected_oid,
            });
            // Atomic CAS `Review -> Done` BEFORE removing any worktree so a
            // concurrent rework (`Review -> Gating`) never loses its worktree.
            if !deps.store.complete_review_task(
                &task.id,
                "pull request squash-merged into the base",
                Some(&merge_payload),
                deps.now(),
            )? {
                continue;
            }
            match deps.git.remove_verified_merged(&wt, expected_oid) {
                Ok(()) => {}
                Err(GitError::Dirty(path)) => {
                    self.summary.errors.push(format!(
                        "{}: leftover: skipped squash-merge cleanup: uncommitted changes in {}",
                        task.id,
                        path.display()
                    ));
                }
                Err(err) => {
                    self.summary.errors.push(format!(
                        "{}: leftover: squash-merge cleanup failed: {err}",
                        task.id
                    ));
                }
            }
            self.clear_restarts_for_task(&task.id);
            self.summary.done.push(task.id);
        }
        Ok(())
    }
}
