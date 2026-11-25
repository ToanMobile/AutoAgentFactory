//! Worktree lookup, merged/squash-merged cleanup, and squash PR verification
//! helpers for [`super::RealGit`].

use std::collections::{HashMap, HashSet};
use std::path::Path;

use aaf_domain::ports::PortResult;

use super::{GitOps, RealGit, err};
use crate::git::worktree::{self, BRANCH_PREFIX};
use crate::git::{GitError, TaskWorktree};
use crate::process::CommandRunner;

impl RealGit {
    /// Runs squash-merge PR verification using `runner` (injectable in tests).
    pub fn verify_squash_merges_with(
        &self,
        candidates: &[(String, String)],
        runner: &dyn CommandRunner,
    ) -> Result<HashMap<String, String>, GitError> {
        self.verify_squash_merges_with_warnings(candidates, runner, &mut |_| {})
    }

    /// Runs squash-merge PR verification using `runner`, forwarding deduplicated
    /// `gh` warnings to `on_warning`.
    pub fn verify_squash_merges_with_warnings(
        &self,
        candidates: &[(String, String)],
        runner: &dyn CommandRunner,
        on_warning: &mut dyn FnMut(String),
    ) -> Result<HashMap<String, String>, GitError> {
        let fetch = || {
            self.fetch_base()
                .map_err(|e| GitError::Invalid(e.to_string()))
        };
        crate::gc::verify_squash_merges_with_warnings(
            &self.repo_root,
            &self.base.branch,
            &self.start(),
            candidates,
            &fetch,
            runner,
            on_warning,
        )
    }

    /// The task's worktree when git has it registered on the task's branch.
    pub(super) fn existing(&self, task_id: &str) -> PortResult<Option<TaskWorktree>> {
        let path =
            worktree::worktree_path(&self.repo_root, &self.worktrees_dir, task_id).map_err(err)?;
        let branch = format!("{BRANCH_PREFIX}{task_id}");
        // Git lists resolved paths; the configured directory may be a link.
        let Ok(resolved) = std::fs::canonicalize(&path) else {
            return Ok(None);
        };
        let registered = worktree::list(&self.repo_root)
            .map_err(err)?
            .into_iter()
            .any(|entry| {
                entry.branch.as_deref() == Some(branch.as_str())
                    && std::fs::canonicalize(&entry.path).is_ok_and(|p| p == resolved)
            });
        Ok(registered.then_some(TaskWorktree { path, branch }))
    }

    pub(super) fn cleanup_impl(&self, task_id: &str) -> PortResult<()> {
        let _admin = self.admin();
        match self.existing(task_id)? {
            // Not forced: refuses a dirty tree and an unmerged branch.
            Some(tree) => worktree::remove(&self.repo_root, &tree, true, false).map_err(err),
            None => worktree::cleanup_missing_worktree(
                &self.repo_root,
                &self.worktrees_dir,
                task_id,
                &self.start(),
            )
            .map_err(err),
        }
    }

    pub(super) fn cleanup_verified_merged_impl(&self, task_id: &str) -> PortResult<()> {
        let _admin = self.admin();
        worktree::cleanup_verified_merged_task(&self.repo_root, &self.worktrees_dir, task_id)
            .map_err(err)
    }

    pub(super) fn sweep_offline_done_impl(
        &self,
        done_task_ids: &[String],
        active_ids: &HashSet<String>,
        cwd: Option<&Path>,
    ) -> PortResult<Vec<String>> {
        let _admin = self.admin();
        let data_dir = self.locks_dir.parent().unwrap_or(&self.locks_dir);
        let effective_cwd = cwd.or(self.cwd.as_deref());
        let report = crate::gc::clean_repo_offline_unlocked(
            &self.repo_root,
            data_dir,
            &self.worktrees_dir,
            done_task_ids,
            active_ids,
            effective_cwd,
        );
        Ok(report.warnings)
    }
}
