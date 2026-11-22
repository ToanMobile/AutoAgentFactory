//! One task = one worktree = one branch `aaf/<task_id>`.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use super::cmd::{failure, git, git_raw};
use super::{GitError, path_str};

pub const BRANCH_PREFIX: &str = "aaf/";
pub const SNAPSHOT_REF_PREFIX: &str = "refs/aaf/snapshots/";
const BATCH_PREFIX: &str = "batch-";

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
    let task_id = worktree.task_id()?;
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
        delete_snapshot_refs(repo_root, task_id)?;
    }
    Ok(())
}

/// Normal merged-task cleanup: removes the clean worktree, its merged branch,
/// and any `refs/aaf/snapshots/<task_id>/*` refs.
pub fn remove_merged(repo_root: &Path, worktree: &TaskWorktree) -> Result<(), GitError> {
    remove(repo_root, worktree, true, false)
}

fn is_valid_oid(oid: &str) -> bool {
    (40..=64).contains(&oid.len()) && oid.bytes().all(|b| b.is_ascii_hexdigit())
}

fn delete_branch_ignore_missing(repo_root: &Path, branch: &str) -> Result<(), GitError> {
    let args = ["branch", "-D", branch];
    let del = git_raw(repo_root, &args)?;
    if !del.success() && !del.stderr.to_ascii_lowercase().contains("not found") {
        return Err(failure(&args, &del));
    }
    Ok(())
}

/// Deletes every `refs/aaf/snapshots/<task_id>/*` ref in `repo_root`.
/// Works even when the task's worktree directory has already been removed.
pub fn delete_snapshot_refs(repo_root: &Path, task_id: &str) -> Result<usize, GitError> {
    validate_task_id(task_id)?;
    let prefix = format!("{SNAPSHOT_REF_PREFIX}{task_id}/");
    let listed = git(repo_root, &["for-each-ref", "--format=%(refname)", &prefix])?;
    let mut deleted = 0usize;
    for refname in listed.lines().filter(|line| !line.is_empty()) {
        if !refname.starts_with(&prefix) {
            continue;
        }
        git(repo_root, &["update-ref", "-d", refname])?;
        deleted += 1;
    }
    Ok(deleted)
}

/// Removes a worktree and branch whose PR was externally verified as
/// squash-merged at `expected_head_oid`.
///
/// Fails closed: refuses dirty worktrees (`GitError::Dirty`), refuses HEAD
/// mismatches against `expected_head_oid`, never passes `--force` to
/// `git worktree remove`, deletes the branch (skipping `--contains`), and
/// deletes `refs/aaf/snapshots/<task_id>/*`.
pub fn remove_verified_merged(
    repo_root: &Path,
    worktree: &TaskWorktree,
    expected_head_oid: &str,
) -> Result<(), GitError> {
    let task_id = worktree.task_id()?;
    if !is_valid_oid(expected_head_oid) {
        return Err(GitError::Invalid(format!(
            "invalid expected head oid {expected_head_oid:?}"
        )));
    }
    if worktree.path.exists() {
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
        if !git(&worktree.path, &["status", "--porcelain"])?.is_empty() {
            return Err(GitError::Dirty(worktree.path.clone()));
        }
        let actual_head = git(&worktree.path, &["rev-parse", "HEAD"])?;
        if actual_head != expected_head_oid {
            return Err(GitError::Command {
                command: "git rev-parse HEAD".to_string(),
                code: Some(0),
                stderr: format!(
                    "HEAD mismatch for {}: expected {expected_head_oid}, got {actual_head}",
                    worktree.branch
                ),
            });
        }
        let path = path_str(&worktree.path)?;
        git(repo_root, &["worktree", "remove", path])?;
    } else {
        prune(repo_root)?;
        let branch_ref = format!("refs/heads/{}", worktree.branch);
        let rev = git_raw(
            repo_root,
            &["rev-parse", "--verify", "--quiet", &branch_ref],
        )?;
        if rev.success() {
            let branch_oid = rev.stdout.trim();
            if branch_oid != expected_head_oid {
                return Err(GitError::Command {
                    command: format!("git rev-parse {branch_ref}"),
                    code: Some(0),
                    stderr: format!(
                        "branch HEAD mismatch for {}: expected {expected_head_oid}, got {branch_oid}",
                        worktree.branch
                    ),
                });
            }
        }
    }
    delete_branch_ignore_missing(repo_root, &worktree.branch)?;
    delete_snapshot_refs(repo_root, task_id)?;
    Ok(())
}

/// Returns the task's existing worktree and its `HEAD` oid (or its local
/// branch tip if the worktree directory is gone) without creating a worktree.
pub fn existing_head(
    repo_root: &Path,
    worktrees_dir: &Path,
    task_id: &str,
) -> Result<Option<(TaskWorktree, String)>, GitError> {
    let path = worktree_path(repo_root, worktrees_dir, task_id)?;
    let branch = format!("{BRANCH_PREFIX}{task_id}");
    if let Ok(resolved) = fs::canonicalize(&path) {
        let registered = list(repo_root)?.into_iter().any(|entry| {
            entry.branch.as_deref() == Some(branch.as_str())
                && fs::canonicalize(&entry.path).is_ok_and(|p| p == resolved)
        });
        if registered {
            let head = git(&path, &["rev-parse", "HEAD"])?;
            return Ok(Some((TaskWorktree { path, branch }, head)));
        }
    }
    let branch_ref = format!("refs/heads/{branch}");
    let out = git_raw(
        repo_root,
        &["rev-parse", "--verify", "--quiet", &branch_ref],
    )?;
    if out.success() {
        let head = out.stdout.trim().to_string();
        if !head.is_empty() {
            return Ok(Some((TaskWorktree { path, branch }, head)));
        }
    }
    Ok(None)
}

/// Inspects `<worktrees_root>/<repo_id>/` and local `aaf/batch-*` refs for
/// orphaned integration worktrees/branches not present in `active_task_ids`.
/// Dirty worktrees are skipped rather than forced. Returns the removed (or
/// removable, when `dry_run` is true) batch names.
pub fn remove_orphan_batch_worktrees(
    repo_root: &Path,
    worktrees_root: &Path,
    repo_id: &str,
    active_task_ids: &HashSet<String>,
    dry_run: bool,
) -> Result<Vec<String>, GitError> {
    let mut candidates = BTreeSet::new();
    let repo_wt_dir = worktrees_root.join(repo_id);
    if let Ok(entries) = fs::read_dir(&repo_wt_dir) {
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name();
            if let Some(id) = name.to_str()
                && id.starts_with(BATCH_PREFIX)
                && validate_task_id(id).is_ok()
                && !active_task_ids.contains(id)
            {
                candidates.insert(id.to_string());
            }
        }
    }
    let branch_refs = git(
        repo_root,
        &[
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/heads/aaf/batch-*",
        ],
    )?;
    for short in branch_refs.lines().filter(|l| !l.is_empty()) {
        if let Some(id) = short.strip_prefix(BRANCH_PREFIX)
            && id.starts_with(BATCH_PREFIX)
            && validate_task_id(id).is_ok()
            && !active_task_ids.contains(id)
        {
            candidates.insert(id.to_string());
        }
    }
    let registered = list(repo_root)?;
    for entry in &registered {
        if let Some(id) = entry.task_id()
            && id.starts_with(BATCH_PREFIX)
            && !active_task_ids.contains(id)
        {
            candidates.insert(id.to_string());
        }
    }

    let mut removed = Vec::new();
    for id in candidates {
        let branch = format!("{BRANCH_PREFIX}{id}");
        let expected_path = repo_wt_dir.join(&id);
        let reg_entry = registered
            .iter()
            .find(|e| e.branch.as_deref() == Some(branch.as_str()));
        if let Some(entry) = reg_entry {
            if entry.path.exists() {
                if !git(&entry.path, &["status", "--porcelain"])?.is_empty() {
                    continue;
                }
                if !dry_run {
                    git(repo_root, &["worktree", "remove", path_str(&entry.path)?])?;
                }
            } else if !dry_run {
                prune(repo_root)?;
            }
        } else if expected_path.exists() {
            let Ok(meta) = fs::symlink_metadata(&expected_path) else {
                continue;
            };
            if meta.file_type().is_symlink() || !meta.is_dir() {
                continue;
            }
            if expected_path.join(".git").exists() {
                let status = git_raw(&expected_path, &["status", "--porcelain"])?;
                if status.success() && !status.stdout.trim().is_empty() {
                    continue;
                }
                if !dry_run {
                    fs::remove_dir_all(&expected_path)?;
                }
            } else {
                let is_empty = fs::read_dir(&expected_path)?.next().transpose()?.is_none();
                if !is_empty {
                    continue;
                }
                if !dry_run {
                    fs::remove_dir(&expected_path)?;
                }
            }
        }
        if !dry_run {
            delete_branch_ignore_missing(repo_root, &branch)?;
            let _ = delete_snapshot_refs(repo_root, &id);
        }
        removed.push(id);
    }
    Ok(removed)
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
