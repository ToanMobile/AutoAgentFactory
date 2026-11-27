//! Git layer: one worktree and one `aaf/<task_id>` branch per task, a
//! per-repo merge queue and pull-request creation (docs/PLAN.md C.8).
//!
//! Every git call goes through [`cmd`], so none can trigger an automatic gc
//! or be redirected by a `GIT_DIR` inherited from a hook.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub mod base_ref;
pub mod cmd;
pub mod gh_client;
pub mod hydration;
pub mod merge_queue;
pub mod pr;
pub mod pr_feedback_gh;
pub mod pr_merged_gh;
pub(crate) mod pseudo_ref_guard;
mod reflog_guard;
pub mod warm_slot;
mod warm_slot_ops;
pub mod worktree;
mod worktree_inspect;

#[cfg(test)]
mod tests;

pub use gh_client::{GhClient, MergeCommitRef, MergeStrategy, PrCheck, PrInfo, SystemGh};
pub use hydration::{
    GateSecretStage, HydratedPaths, hydrate_for_agent, hydrate_fresh_worktree_with_snaps,
    stage_for_gate,
};
pub use merge_queue::{MergeOutcome, MergeRequest, merge};
pub use pr::{PrOutcome, PrRequest, open_pr, open_pr_with_proof, pr_body, pr_body_with_proof};
pub use reflog_guard::ensure_plain_reflogs;
pub(crate) use reflog_guard::ensure_plain_reflogs_all;
pub use warm_slot::{
    SlotKind, WarmSlot, base_snapshots_dir, promote_clean_base_snapshot,
    promote_from_pure_base_slot, restore_clean_base_snapshot, warm_slots_root,
};
pub use worktree::{TaskWorktree, WorktreeEntry};

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("invalid task id {0:?}: expected 1 to 64 characters of [A-Za-z0-9_-]")]
    InvalidTaskId(String),
    #[error("invalid argument: {0}")]
    Invalid(String),
    #[error("already exists: {0}")]
    AlreadyExists(String),
    #[error("uncommitted changes in {}", .0.display())]
    Dirty(PathBuf),
    #[error("branch {0} has commits that no other branch contains")]
    Unmerged(String),
    #[error("timed out waiting for the merge lock {}", .0.display())]
    LockTimeout(PathBuf),
    #[error("the gate could not run: {0}")]
    Gate(String),
    #[error("`{command}` timed out")]
    Timeout { command: String },
    #[error("`{command}` failed (exit {code:?}): {stderr}")]
    Command {
        command: String,
        code: Option<i32>,
        stderr: String,
    },
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

impl From<crate::confine_core::ConfineError> for GitError {
    fn from(err: crate::confine_core::ConfineError) -> Self {
        GitError::Invalid(err.to_string())
    }
}

/// Resolves the main worktree root for `path`, even when invoked inside a linked worktree.
pub fn main_work_tree(path: &Path) -> PathBuf {
    if let Some(main) = aaf_fp::repository_dirs(path).into_iter().last() {
        return main;
    }
    if let Ok(common) = cmd::git(
        path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ) {
        let common_path = PathBuf::from(common.trim());
        if common_path.file_name().is_some_and(|n| n == ".git")
            && let Some(parent) = common_path.parent()
        {
            return fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
        }
    }
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Case-insensitive ASCII protection for base/default/release/public/non-ASCII branches.
pub(crate) fn is_protected_branch(branch: &str, base_ref_name: &str) -> bool {
    let trimmed = branch.trim();
    if trimmed.is_empty()
        || !trimmed.is_ascii()
        || trimmed.eq_ignore_ascii_case(base_ref_name.trim())
    {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "main" | "master" | "develop" | "development" | "trunk" | "head"
    ) || lower.starts_with("release/")
        || lower.starts_with("public-")
}

/// Git arguments are passed as `&str`; a path that is not UTF-8 is refused
/// instead of being silently mangled.
pub(crate) fn path_str(path: &Path) -> Result<&str, GitError> {
    path.to_str()
        .ok_or_else(|| GitError::Invalid(format!("path is not UTF-8: {}", path.display())))
}

/// Rejects a branch name git would not accept, and anything git could read
/// as an option.
pub(crate) fn check_branch_name(repo: &Path, name: &str) -> Result<(), GitError> {
    let full = format!("refs/heads/{name}");
    let valid = !name.is_empty()
        && !name.starts_with('-')
        && cmd::git_raw(repo, &["check-ref-format", &full])?.success();
    if valid {
        Ok(())
    } else {
        Err(GitError::Invalid(format!("branch name {name:?}")))
    }
}

/// True when `remote` is configured in `repo`.
pub(crate) fn has_remote(repo: &Path, remote: &str) -> Result<bool, GitError> {
    if remote.is_empty() || remote.starts_with('-') {
        return Err(GitError::Invalid(format!("remote name {remote:?}")));
    }
    Ok(cmd::git(repo, &["remote"])?.lines().any(|l| l == remote))
}
