//! Worktree lookup, merged/squash-merged cleanup, and squash PR verification
//! helpers for [`super::RealGit`].

use std::collections::{HashMap, HashSet};
use std::path::Path;

use aaf_domain::ports::PortResult;

use super::{GitOps, RealGit, err};
use crate::git::cmd::git;
use crate::git::pr_feedback_gh::PrRef;
use crate::git::pr_merged_gh::{VerifiedPrMerge, verify_merged_pr};
use crate::git::worktree;
use crate::git::{GitError, TaskWorktree};
use crate::process::CommandRunner;

impl RealGit {
    /// Checks whether a recorded PR URL is `MERGED` into the configured base
    /// branch using `runner` (injectable in tests).
    pub fn verify_merged_pr_with(
        &self,
        pr: &PrRef,
        runner: &dyn CommandRunner,
    ) -> Result<Option<VerifiedPrMerge>, GitError> {
        verify_merged_pr(
            runner,
            &self.repo_root,
            pr,
            &self.base.branch,
            &self.start(),
        )
    }
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

    /// The task's worktree when git has it registered on the task's branch
    /// (in `worktrees_dir` or an AAF-managed `<repo_id>/<task_id>` path from a
    /// previous `AAF_WORKTREE_DIR`).
    pub(super) fn existing(&self, task_id: &str) -> PortResult<Option<TaskWorktree>> {
        let found = worktree::existing_registered(&self.repo_root, &self.worktrees_dir, task_id)
            .map_err(err)?;
        Ok(found.map(|(wt, _)| wt))
    }

    /// Re-attaches an existing unattached `aaf/<task_id>` branch into
    /// `worktrees_dir` and hydrates it from the trusted base commit.
    pub(super) fn reattach_existing_branch(
        &self,
        task_id: &str,
    ) -> PortResult<Option<TaskWorktree>> {
        let base_spec = format!("{}^{{commit}}", self.start());
        let base_sha = git(&self.repo_root, &["rev-parse", "--verify", &base_spec]).map_err(err)?;
        let Some(wt) = worktree::attach_existing_branch(
            &self.repo_root,
            &self.worktrees_dir,
            task_id,
            Some(&base_sha),
        )
        .map_err(err)?
        else {
            return Ok(None);
        };
        let data_dir = self.locks_dir.parent();
        crate::git::hydration::hydrate_reattached_worktree_with_data_dir(
            &self.repo_root,
            &wt,
            &base_sha,
            data_dir,
        )
        .map_err(err)?;
        Ok(Some(wt))
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

    pub(super) fn cleanup_verified_merged_impl(
        &self,
        task_id: &str,
        expected_head: &str,
    ) -> PortResult<()> {
        let _admin = self.admin();
        worktree::cleanup_verified_merged_task(
            &self.repo_root,
            &self.worktrees_dir,
            task_id,
            expected_head,
        )
        .map_err(err)
    }

    pub(super) fn sweep_offline_done_impl(
        &self,
        done_task_ids: &[String],
        active_ids: &HashSet<String>,
        cwd: Option<&Path>,
    ) -> PortResult<Vec<String>> {
        let done_tasks: Vec<(String, Option<String>)> =
            done_task_ids.iter().map(|id| (id.clone(), None)).collect();
        self.sweep_offline_done_with_heads_impl(&done_tasks, active_ids, cwd)
    }

    pub(super) fn sweep_offline_done_with_heads_impl(
        &self,
        done_tasks: &[(String, Option<String>)],
        active_ids: &HashSet<String>,
        cwd: Option<&Path>,
    ) -> PortResult<Vec<String>> {
        let _admin = self.admin();
        let data_dir = self.locks_dir.parent().unwrap_or(&self.locks_dir);
        let effective_cwd = cwd.or(self.cwd.as_deref());
        let report = crate::gc::clean_repo_offline_unlocked_with_heads(
            &self.repo_root,
            data_dir,
            &self.worktrees_dir,
            done_tasks,
            active_ids,
            effective_cwd,
        );
        Ok(report.warnings)
    }
}
