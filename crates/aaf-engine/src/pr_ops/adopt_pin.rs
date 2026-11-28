//! Pinning an adopted head: `refs/heads/aaf/<id>` at the commit that was
//! checked, and a worktree on that branch where the lane can find it. Both are
//! made before the task is inserted and unmade when it is not, so a failed
//! adoption leaves no ref and no directory behind.

use std::fs;
use std::path::Path;

use crate::git::cmd::{git, git_raw};
use crate::git::worktree::{self, validate_task_id};
use crate::git::{GitError, TaskWorktree, ensure_plain_reflogs};
use aaf_domain::engine_contracts::BRANCH_PREFIX;

/// Creates `refs/heads/aaf/<task_id>` at `oid` (it must not exist yet) and a
/// worktree `<worktrees_dir>/<repo_id>/<task_id>` checked out on it. Any
/// failure removes whatever this call made.
pub fn pin_head(
    repo_root: &Path,
    worktrees_dir: &Path,
    task_id: &str,
    oid: &str,
) -> Result<TaskWorktree, GitError> {
    validate_task_id(task_id)?;
    let path = worktree::worktree_path(repo_root, worktrees_dir, task_id)?;
    if path.exists() {
        return Err(GitError::AlreadyExists(path.display().to_string()));
    }
    let branch = format!("{BRANCH_PREFIX}{task_id}");
    let branch_ref = format!("refs/heads/{branch}");
    // The empty old value: the ref must not exist.
    git(repo_root, &["update-ref", &branch_ref, oid, ""])?;
    let tree = TaskWorktree {
        path: path.clone(),
        branch: branch.clone(),
    };
    let made = (|| -> Result<(), GitError> {
        let parent = path.parent().ok_or_else(|| {
            GitError::Invalid(format!("worktree path {} has no parent", path.display()))
        })?;
        fs::create_dir_all(parent)?;
        ensure_plain_reflogs(repo_root)?;
        let path_arg = crate::git::path_str(&path)?;
        // No hooks while the PR's files are checked out: a `core.hooksPath` that is
        // relative to the work tree would run the PR's own `post-checkout`.
        let args = [
            "-c",
            "core.hooksPath=/dev/null",
            "worktree",
            "add",
            path_arg,
            &branch,
        ];
        git(repo_root, &args)?;
        // What the lane will check before it does anything.
        let head = git(&path, &["rev-parse", "HEAD"])?;
        if !head.eq_ignore_ascii_case(oid) {
            return Err(GitError::Invalid(format!(
                "the new worktree is at {head}, not at {oid}"
            )));
        }
        Ok(())
    })();
    match made {
        Ok(()) => Ok(tree),
        Err(err) => {
            unpin_head(repo_root, &tree, oid);
            Err(err)
        }
    }
}

/// Removes what [`pin_head`] made: the worktree, then the ref, but only while
/// the ref still names `oid` (a branch someone moved is not ours to delete).
/// Best effort; there is nobody to tell about a failure here.
pub fn unpin_head(repo_root: &Path, tree: &TaskWorktree, oid: &str) {
    if tree.path.exists()
        && let Some(path) = tree.path.to_str()
    {
        let _ = git_raw(repo_root, &["worktree", "remove", "--force", path]);
        if tree.path.exists() {
            let _ = fs::remove_dir_all(&tree.path);
        }
    }
    let _ = git_raw(repo_root, &["worktree", "prune"]);
    let branch_ref = format!("refs/heads/{}", tree.branch);
    let _ = git_raw(repo_root, &["update-ref", "-d", &branch_ref, oid]);
}
