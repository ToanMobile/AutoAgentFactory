//! Base-drift detection and cross-branch (`aaf/*`) ancestry guards for `RealGit`.

use aaf_domain::ports::PortResult;

use super::{RealGit, err};
use crate::git::TaskWorktree;
use crate::git::cmd::{git, git_raw};
use crate::git::worktree::BRANCH_PREFIX;

impl RealGit {
    /// Files modified on the base branch between `merge-base(base, HEAD)` and
    /// `base` tip. Empty when the worktree is already based on the current tip.
    pub(super) fn base_changed_paths_impl(
        &self,
        worktree: &TaskWorktree,
    ) -> PortResult<Vec<String>> {
        let start = self.start();
        let tree = worktree.path.as_path();
        let base_tip = git(tree, &["rev-parse", "--verify", &start]).map_err(err)?;
        let merge_base = git(tree, &["merge-base", &start, "HEAD"]).map_err(err)?;
        if merge_base.trim() == base_tip.trim() {
            return Ok(Vec::new());
        }
        let range = format!("{}..{}", merge_base.trim(), base_tip.trim());
        let raw = git(tree, &["diff", "--name-only", &range]).map_err(err)?;
        Ok(raw
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// Foreign `aaf/*` task branches whose unmerged tip commit is an ancestor of
    /// `worktree`'s `HEAD` (e.g. a subtask that ran `git merge aaf/<parent>`).
    ///
    /// Ignores `worktree.branch`, `aaf/batch-*`, and any branch whose tip is
    /// already contained in `base` (legitimate merged history).
    pub(super) fn foreign_aaf_branch_ancestors_impl(
        &self,
        worktree: &TaskWorktree,
    ) -> PortResult<Vec<String>> {
        let start = self.start();
        let tree = worktree.path.as_path();
        let prefix_ref = format!("refs/heads/{BRANCH_PREFIX}");
        let raw = git(
            tree,
            &[
                "for-each-ref",
                "--format=%(refname:short) %(objectname)",
                &prefix_ref,
            ],
        )
        .map_err(err)?;
        let mut contaminated = Vec::new();
        for line in raw.lines() {
            let Some((branch, oid)) = line.trim().split_once(' ') else {
                continue;
            };
            if branch == worktree.branch || branch.starts_with("aaf/batch-") || oid.is_empty() {
                continue;
            }
            // Skip branches already merged into base (legitimate base history).
            let in_base = git_raw(tree, &["merge-base", "--is-ancestor", oid, &start])
                .map_err(err)?
                .success();
            if in_base {
                continue;
            }
            let in_head = git_raw(tree, &["merge-base", "--is-ancestor", oid, "HEAD"])
                .map_err(err)?
                .success();
            if in_head {
                contaminated.push(branch.to_string());
            }
        }
        contaminated.sort();
        Ok(contaminated)
    }
}
