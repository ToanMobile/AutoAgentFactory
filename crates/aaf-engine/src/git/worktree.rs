//! One task = one worktree = one branch `aaf/<task_id>`.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use super::cmd::{git, git_raw};
use super::{GitError, path_str};

mod cleanup;
mod submodules;

pub use cleanup::{
    cleanup_missing_worktree, cleanup_verified_merged_task, delete_snapshot_refs,
    is_operation_in_progress, remove_verified_merged,
};
use cleanup::{delete_branch_ignore_missing, is_other_branch_holder};

pub const BRANCH_PREFIX: &str = "aaf/";
pub const SNAPSHOT_REF_PREFIX: &str = "refs/aaf/snapshots/";
const BATCH_PREFIX: &str = "batch-";
pub(super) const SUBMODULES_REFUSAL: &str =
    "working trees containing submodules cannot be moved or removed";

/// Fails with [`GitError::Dirty`] when `tree` differs from its `HEAD`
/// (staged, unstaged or untracked): what the gate reads must be what lands.
pub(crate) fn ensure_clean(tree: &Path) -> Result<(), GitError> {
    if git(tree, &["status", "--porcelain"])?.is_empty() {
        Ok(())
    } else {
        Err(GitError::Dirty(tree.to_path_buf()))
    }
}

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
    /// `git worktree lock`ed: git keeps its record (and its branch checked
    /// out) even when the directory is gone.
    pub locked: bool,
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
/// hand a new task another task's leftovers. Submodules checked out in the
/// main checkout are checked out too; failing that is only a warning.
pub fn create(
    repo_root: &Path,
    worktrees_dir: &Path,
    task_id: &str,
    base_ref: &str,
) -> Result<TaskWorktree, GitError> {
    let (tree, warning) = create_with_warning(repo_root, worktrees_dir, task_id, base_ref)?;
    if let Some(warning) = warning {
        eprintln!("aaf: warning: {warning}");
    }
    Ok(tree)
}

/// [`create`], handing back the submodule warning instead of printing it.
pub fn create_with_warning(
    repo_root: &Path,
    worktrees_dir: &Path,
    task_id: &str,
    base_ref: &str,
) -> Result<(TaskWorktree, Option<String>), GitError> {
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
    let warning = submodules::init(repo_root, &path);
    Ok((TaskWorktree { path, branch }, warning))
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
        ensure_clean(&worktree.path)?;
        if is_operation_in_progress(&worktree.path)? {
            return Err(GitError::Invalid(format!(
                "git operation in progress in {}",
                worktree.path.display()
            )));
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
            if !holders
                .lines()
                .any(|name| is_other_branch_holder(name, &worktree.branch, &branch_ref))
            {
                return Err(GitError::Unmerged(worktree.branch.clone()));
            }
        }
    }
    let path = path_str(&worktree.path)?;
    // Clean, submodule contents included whatever `.gitmodules` says to
    // ignore: git's blanket refusal of checked-out submodules protects
    // nothing then.
    let submodules =
        !force && removal_refusal(&worktree.path).is_ok_and(|r| r == Some(SUBMODULES_REFUSAL));
    if submodules {
        let args = ["status", "--porcelain", "--ignore-submodules=none"];
        if !git(&worktree.path, &args)?.is_empty() {
            return Err(GitError::Dirty(worktree.path.clone()));
        }
    }
    if force || submodules {
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

/// Returns the task's registered worktree and its `HEAD` oid only when the
/// worktree directory actually exists on disk (`path.is_dir()`). Unlike
/// [`existing_head`], never falls back to a branch-only ref when the directory
/// has been removed.
pub fn existing_registered(
    repo_root: &Path,
    worktrees_dir: &Path,
    task_id: &str,
) -> Result<Option<(TaskWorktree, String)>, GitError> {
    let path = worktree_path(repo_root, worktrees_dir, task_id)?;
    if !path.is_dir() {
        return Ok(None);
    }
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
    Ok(None)
}

/// Returns the task's existing worktree and its `HEAD` oid (or its local
/// branch tip if the worktree directory is gone) without creating a worktree.
pub fn existing_head(
    repo_root: &Path,
    worktrees_dir: &Path,
    task_id: &str,
) -> Result<Option<(TaskWorktree, String)>, GitError> {
    if let Some(found) = existing_registered(repo_root, worktrees_dir, task_id)? {
        return Ok(Some(found));
    }
    let path = worktree_path(repo_root, worktrees_dir, task_id)?;
    let branch = format!("{BRANCH_PREFIX}{task_id}");
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

/// Outcome of [`remove_orphan_batch_worktrees`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrphanSweep {
    /// Removed (or removable, on a dry run) batch names.
    pub removed: Vec<String>,
    /// `"<id>: worktree remove: <stderr>"` for each clean orphan git
    /// refused to remove without `--force` (initialised submodules, a lock).
    pub warnings: Vec<String>,
}

/// Inspects `<worktrees_root>/<repo_id>/` and local `aaf/batch-*` refs for
/// orphaned integration worktrees/branches not present in `active_task_ids`.
/// Dirty worktrees, and clean ones `git worktree remove` refuses without
/// `--force`, are skipped rather than forced; a refusal is reported in
/// [`OrphanSweep::warnings`].
pub fn remove_orphan_batch_worktrees(
    repo_root: &Path,
    worktrees_root: &Path,
    repo_id: &str,
    active_task_ids: &HashSet<String>,
    dry_run: bool,
) -> Result<OrphanSweep, GitError> {
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

    let mut sweep = OrphanSweep::default();
    for id in candidates {
        let branch = format!("{BRANCH_PREFIX}{id}");
        let expected_path = repo_wt_dir.join(&id);
        let reg_entry = registered
            .iter()
            .find(|e| e.branch.as_deref() == Some(branch.as_str()));
        if let Some(entry) = reg_entry {
            // Only the factory's own tree is removed: the branch checked out
            // anywhere else (the main worktree, a user's linked worktree) is
            // someone's checkout and is kept.
            if let Some(warning) = foreign_checkout(&id, entry, &registered, &expected_path) {
                sweep.warnings.push(warning);
                continue;
            }
            if entry.path.exists() {
                if !git(&entry.path, &["status", "--porcelain"])?.is_empty() {
                    continue;
                }
                // A clean worktree git still refuses without --force
                // (initialised submodules, a lock) is kept like a dirty
                // one, without stopping the sweep, and reported.
                let refusal = if dry_run {
                    removal_refusal(&entry.path)?
                        .map(|reason| format!("would be refused: {reason}"))
                } else {
                    let out = git_raw(repo_root, &["worktree", "remove", path_str(&entry.path)?])?;
                    (!out.success()).then(|| out.stderr.trim().to_string())
                };
                if let Some(reason) = refusal {
                    sweep
                        .warnings
                        .push(format!("{id}: worktree remove: {reason}"));
                    continue;
                }
            } else if entry.locked {
                // The tree is on a volume that is not mounted: git keeps
                // its record and its branch.
                sweep.warnings.push(format!(
                    "{id}: worktree is locked and its directory is missing: kept"
                ));
                continue;
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
                // A tree git cannot read is kept, like a dirty one.
                let status = git_raw(&expected_path, &["status", "--porcelain"])?;
                if !status.success() || !status.stdout.trim().is_empty() {
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
            // e.g. a stale ref lock: this id's warning, the sweep goes on.
            if let Err(err) = delete_branch_ignore_missing(repo_root, &branch) {
                sweep.warnings.push(format!("{id}: branch delete: {err}"));
                continue;
            }
            let _ = delete_snapshot_refs(repo_root, &id);
        }
        sweep.removed.push(id);
    }
    Ok(sweep)
}

/// `"<id>: branch is checked out in <where>: kept"` when `entry` (the
/// registered tree holding task `id`'s branch) is not the factory's own
/// worktree at `expected` (see [`worktree_path`]); `None` when it is. git
/// lists the main worktree first.
pub(crate) fn foreign_checkout(
    id: &str,
    entry: &WorktreeEntry,
    registered: &[WorktreeEntry],
    expected: &Path,
) -> Option<String> {
    if registered
        .first()
        .is_some_and(|main| main.path == entry.path)
    {
        return Some(format!(
            "{id}: branch is checked out in the main worktree: kept"
        ));
    }
    (resolved(&entry.path) != resolved(expected)).then(|| {
        format!(
            "{id}: branch is checked out in {}, not its aaf worktree: kept",
            entry.path.display()
        )
    })
}

/// The canonical path; a missing one through its canonical parent, so a
/// tree whose directory is gone still matches across symlinked prefixes.
fn resolved(path: &Path) -> PathBuf {
    if let Ok(real) = fs::canonicalize(path) {
        return real;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => {
            fs::canonicalize(parent).map_or_else(|_| path.to_path_buf(), |dir| dir.join(name))
        }
        _ => path.to_path_buf(),
    }
}

pub use super::worktree_inspect::{list, prune, removal_refusal};
