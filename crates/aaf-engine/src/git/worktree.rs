//! One task = one worktree = one branch `aaf/<task_id>`.

use std::fs;
use std::path::{Path, PathBuf};

use super::cmd::{git, git_raw};
use super::{GitError, path_str};

pub const BRANCH_PREFIX: &str = "aaf/";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskWorktree {
    pub path: PathBuf,
    /// Short branch name, `aaf/<task_id>`.
    pub branch: String,
}

impl TaskWorktree {
    /// The task id behind the branch. Fails for any branch this layer could
    /// not have created, so a hand-built value never reaches a git ref.
    pub fn task_id(&self) -> Result<&str, GitError> {
        let id = self
            .branch
            .strip_prefix(BRANCH_PREFIX)
            .ok_or_else(|| GitError::Invalid(format!("not a task branch: {:?}", self.branch)))?;
        validate_task_id(id)?;
        Ok(id)
    }
}

/// One entry of `git worktree list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    pub head: Option<String>,
    /// Short branch name; `None` when detached or bare.
    pub branch: Option<String>,
}

impl WorktreeEntry {
    pub fn task_id(&self) -> Option<&str> {
        let id = self.branch.as_deref()?.strip_prefix(BRANCH_PREFIX)?;
        validate_task_id(id).is_ok().then_some(id)
    }
}

/// `[A-Za-z0-9_-]{1,64}`: the id becomes a path component and a ref name.
pub fn validate_task_id(task_id: &str) -> Result<(), GitError> {
    let valid = (1..=64).contains(&task_id.len())
        && task_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if valid {
        Ok(())
    } else {
        Err(GitError::InvalidTaskId(task_id.to_string()))
    }
}

/// `<worktrees_dir>/<repo_id>/<task_id>`.
pub fn worktree_path(
    repo_root: &Path,
    worktrees_dir: &Path,
    task_id: &str,
) -> Result<PathBuf, GitError> {
    validate_task_id(task_id)?;
    Ok(worktrees_dir.join(aaf_fp::repo_id(repo_root)).join(task_id))
}

/// Creates the task's worktree on a new branch `aaf/<task_id>` at `base_ref`.
/// An existing worktree directory or branch is an error: reusing one would
/// hand a new task another task's leftovers.
pub fn create(
    repo_root: &Path,
    worktrees_dir: &Path,
    task_id: &str,
    base_ref: &str,
) -> Result<TaskWorktree, GitError> {
    let path = worktree_path(repo_root, worktrees_dir, task_id)?;
    let branch = format!("{BRANCH_PREFIX}{task_id}");
    if base_ref.is_empty() || base_ref.starts_with('-') {
        return Err(GitError::Invalid(format!("base ref {base_ref:?}")));
    }
    let commit = format!("{base_ref}^{{commit}}");
    if !git_raw(repo_root, &["rev-parse", "--verify", "--quiet", &commit])?.success() {
        return Err(GitError::Invalid(format!(
            "base ref {base_ref:?} is not a commit"
        )));
    }
    if path.exists() {
        return Err(GitError::AlreadyExists(path.display().to_string()));
    }
    let branch_ref = format!("refs/heads/{branch}");
    if git_raw(repo_root, &["show-ref", "--verify", "--quiet", &branch_ref])?.success() {
        return Err(GitError::AlreadyExists(format!("branch {branch}")));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    git(
        repo_root,
        &["worktree", "add", "-b", &branch, path_str(&path)?, base_ref],
    )?;
    Ok(TaskWorktree { path, branch })
}

/// Removes the worktree and, when asked, its branch.
///
/// Without `force` nothing is lost: uncommitted or untracked work in the
/// worktree refuses with [`GitError::Dirty`], and a branch whose commits no
/// other local or remote-tracking branch contains refuses with
/// [`GitError::Unmerged`]. Both are checked before anything is removed.
pub fn remove(
    repo_root: &Path,
    worktree: &TaskWorktree,
    delete_branch: bool,
    force: bool,
) -> Result<(), GitError> {
    worktree.task_id()?;
    let wanted = fs::canonicalize(&worktree.path)?;
    let registered = list(repo_root)?.into_iter().any(|entry| {
        entry.branch.as_deref() == Some(worktree.branch.as_str())
            && fs::canonicalize(&entry.path).is_ok_and(|p| p == wanted)
    });
    if !registered {
        return Err(GitError::Invalid(format!(
            "{} is not a worktree of branch {}",
            worktree.path.display(),
            worktree.branch
        )));
    }
    let branch_ref = format!("refs/heads/{}", worktree.branch);
    if !force {
        if !git(&worktree.path, &["status", "--porcelain"])?.is_empty() {
            return Err(GitError::Dirty(worktree.path.clone()));
        }
        if delete_branch {
            let holders = git(
                repo_root,
                &[
                    "for-each-ref",
                    "--format=%(refname)",
                    "--contains",
                    &branch_ref,
                    "refs/heads",
                    "refs/remotes",
                ],
            )?;
            if !holders.lines().any(|name| name != branch_ref) {
                return Err(GitError::Unmerged(worktree.branch.clone()));
            }
        }
    }
    let path = path_str(&worktree.path)?;
    if force {
        git(repo_root, &["worktree", "remove", "--force", path])?;
    } else {
        git(repo_root, &["worktree", "remove", path])?;
    }
    if delete_branch {
        // -D because "merged" was decided above against every branch, not
        // only the HEAD that `branch -d` would compare with.
        git(repo_root, &["branch", "-D", &worktree.branch])?;
    }
    Ok(())
}

/// Every worktree of the repository, the main one first.
pub fn list(repo_root: &Path) -> Result<Vec<WorktreeEntry>, GitError> {
    let args = ["worktree", "list", "--porcelain", "-z"];
    let out = git_raw(repo_root, &args)?;
    if !out.success() {
        return Err(super::cmd::failure(&args, &out));
    }
    let mut entries: Vec<WorktreeEntry> = Vec::new();
    for field in out.stdout.split('\0') {
        if let Some(path) = field.strip_prefix("worktree ") {
            entries.push(WorktreeEntry {
                path: PathBuf::from(path),
                head: None,
                branch: None,
            });
        } else if let Some(entry) = entries.last_mut() {
            if let Some(head) = field.strip_prefix("HEAD ") {
                entry.head = Some(head.to_string());
            } else if let Some(name) = field.strip_prefix("branch refs/heads/") {
                entry.branch = Some(name.to_string());
            }
        }
    }
    Ok(entries)
}

/// Drops the records of worktrees whose directory is gone.
pub fn prune(repo_root: &Path) -> Result<(), GitError> {
    git(repo_root, &["worktree", "prune"]).map(|_| ())
}
