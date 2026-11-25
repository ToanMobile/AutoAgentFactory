//! One listing of the task branch and snapshot ref namespaces per
//! repository: gc looks a Done task's leftovers up here instead of asking git
//! twice for every task ever recorded.

use std::collections::HashMap;
use std::path::Path;

use crate::git::GitError;
use crate::git::cmd::git;
use crate::git::worktree::{BRANCH_PREFIX, SNAPSHOT_REF_PREFIX};

const MERGE_REF_PREFIX: &str = "refs/aaf-merge/";

/// The task branches (`refs/heads/aaf/*` -> tip OID), snapshot ref counts per
/// task id (`refs/aaf/snapshots/<id>/*`), and temporary `refs/aaf-merge/*` refs.
#[derive(Debug, Default)]
pub(super) struct RepoRefs {
    branches: HashMap<String, String>,
    snapshots: HashMap<String, usize>,
    merge_refs: Vec<String>,
}

impl RepoRefs {
    pub(super) fn list(repo: &Path) -> Result<Self, GitError> {
        let branches = format!("refs/heads/{BRANCH_PREFIX}");
        let listed = git(
            repo,
            &[
                "for-each-ref",
                "--format=%(refname)%09%(objectname)",
                &branches,
                SNAPSHOT_REF_PREFIX,
                MERGE_REF_PREFIX,
            ],
        )?;
        Ok(Self::parse(&listed))
    }

    fn parse(listed: &str) -> Self {
        let mut refs = Self::default();
        for line in listed.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let (name, oid) = line.split_once('\t').unwrap_or((line, ""));
            if let Some(rest) = name.strip_prefix(SNAPSHOT_REF_PREFIX) {
                // Only refs under `<id>/`, as a per-task listing matched.
                if let Some((id, tail)) = rest.split_once('/')
                    && !id.is_empty()
                    && !tail.is_empty()
                {
                    *refs.snapshots.entry(id.to_string()).or_default() += 1;
                }
            } else if name.starts_with(MERGE_REF_PREFIX) {
                refs.merge_refs.push(name.to_string());
            } else if name.starts_with("refs/heads/") {
                refs.branches
                    .insert(name.to_string(), oid.trim().to_string());
            }
        }
        refs
    }

    /// Whether git listed the full ref `branch_ref` (`refs/heads/aaf/<id>`).
    pub(super) fn has_branch(&self, branch_ref: &str) -> bool {
        self.branches.contains_key(branch_ref)
    }

    /// The OID recorded for `branch_ref` during [`RepoRefs::list`], if non-empty.
    pub(super) fn branch_oid(&self, branch_ref: &str) -> Option<&str> {
        self.branches
            .get(branch_ref)
            .map(String::as_str)
            .filter(|oid| !oid.is_empty())
    }

    /// All listed `refs/heads/aaf/<id>` full branch refs in deterministic order.
    pub(super) fn branch_refs(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.branches.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }

    /// Short names (`aaf/batch-*`) of all listed batch branches.
    pub(super) fn batch_short_branches(&self) -> Vec<String> {
        let prefix = format!("refs/heads/{BRANCH_PREFIX}batch-");
        self.branches
            .keys()
            .filter(|full| full.starts_with(&prefix))
            .filter_map(|full| full.strip_prefix("refs/heads/").map(str::to_string))
            .collect()
    }

    /// Listed `refs/aaf-merge/*` full ref names.
    pub(super) fn merge_refs(&self) -> &[String] {
        &self.merge_refs
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
