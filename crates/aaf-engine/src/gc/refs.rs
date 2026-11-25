//! One listing of the task branch and snapshot ref namespaces per
//! repository: gc looks a Done task's leftovers up here instead of asking git
//! twice for every task ever recorded.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::git::GitError;
use crate::git::cmd::git;
use crate::git::worktree::{BRANCH_PREFIX, SNAPSHOT_REF_PREFIX};

/// The task branches (`refs/heads/aaf/*`) and the number of snapshot refs
/// per task id (`refs/aaf/snapshots/<id>/*`) git listed.
#[derive(Debug, Default)]
pub(super) struct RepoRefs {
    branches: HashSet<String>,
    snapshots: HashMap<String, usize>,
}

impl RepoRefs {
    pub(super) fn list(repo: &Path) -> Result<Self, GitError> {
        let branches = format!("refs/heads/{BRANCH_PREFIX}");
        let listed = git(
            repo,
            &[
                "for-each-ref",
                "--format=%(refname)",
                &branches,
                SNAPSHOT_REF_PREFIX,
            ],
        )?;
        Ok(Self::parse(&listed))
    }

    fn parse(listed: &str) -> Self {
        let mut refs = Self::default();
        for name in listed.lines().map(str::trim).filter(|l| !l.is_empty()) {
            if let Some(rest) = name.strip_prefix(SNAPSHOT_REF_PREFIX) {
                // Only refs under `<id>/`, as a per-task listing matched.
                if let Some((id, tail)) = rest.split_once('/')
                    && !id.is_empty()
                    && !tail.is_empty()
                {
                    *refs.snapshots.entry(id.to_string()).or_default() += 1;
                }
            } else if name.starts_with("refs/heads/") {
                refs.branches.insert(name.to_string());
            }
        }
        refs
    }

    /// Whether git listed the full ref `branch_ref` (`refs/heads/aaf/<id>`).
    pub(super) fn has_branch(&self, branch_ref: &str) -> bool {
        self.branches.contains(branch_ref)
    }

    /// All listed `refs/heads/aaf/<id>` full branch refs in deterministic order.
    pub(super) fn branch_refs(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.branches.iter().map(String::as_str).collect();
        names.sort_unstable();
        names
    }

    /// How many snapshot refs git listed under `task_id/`.
    pub(super) fn snapshot_count(&self, task_id: &str) -> usize {
        self.snapshots.get(task_id).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_counts_per_task_and_keeps_prefix_neighbours_apart() {
        let refs = RepoRefs::parse(
            "refs/heads/aaf/x\nrefs/heads/aaf/x-extra\n\
             refs/aaf/snapshots/x/1\nrefs/aaf/snapshots/x/2\n\
             refs/aaf/snapshots/x-extra/1\nrefs/aaf/snapshots/y/a/b\n\
             refs/aaf/snapshots/z\n\n",
        );
        assert!(refs.has_branch("refs/heads/aaf/x"));
        assert!(refs.has_branch("refs/heads/aaf/x-extra"));
        assert!(!refs.has_branch("refs/heads/aaf/y"));
        assert_eq!(refs.snapshot_count("x"), 2);
        assert_eq!(refs.snapshot_count("x-extra"), 1);
        assert_eq!(refs.snapshot_count("y"), 1);
        assert_eq!(refs.snapshot_count("z"), 0);
        assert_eq!(refs.snapshot_count("w"), 0);
    }
}
