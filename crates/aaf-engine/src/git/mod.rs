//! Git layer: one worktree and one `aaf/<task_id>` branch per task, a
//! per-repo merge queue and pull-request creation (docs/PLAN.md C.8).
//!
//! Every git call goes through [`cmd`], so none can trigger an automatic gc
//! or be redirected by a `GIT_DIR` inherited from a hook.

use std::io;
use std::path::{Path, PathBuf};

pub mod cmd;
pub mod gh_client;
pub mod merge_queue;
pub mod pr;
pub mod pr_merge;
mod pr_merge_support;
pub mod worktree;

#[cfg(test)]
mod tests;

pub use gh_client::{GhClient, MergeStrategy, PrCheck, PrInfo, SystemGh};
pub use merge_queue::{MergeOutcome, MergeRequest, merge};
pub use pr::{PrOutcome, PrRequest, open_pr, open_pr_with_proof, pr_body, pr_body_with_proof};
pub use pr_merge::{PrMergeOutcome, PrMergeRequest, merge_pr};
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
