//! One task = one worktree = one branch `aaf/<task_id>`.

use std::fs;
use std::path::{Path, PathBuf};

use super::cmd::{git, git_raw};
use super::{GitError, path_str};

mod batch;
mod cleanup;
mod reattach;
mod submodules;

pub(crate) use batch::remove_orphan_batch_worktrees_prepared;
pub use batch::{
    OrphanBatchSweep, OrphanSweep, parse_batch_pid, process_alive, remove_orphan_batch_worktrees,
    remove_orphan_batch_worktrees_dead_pid_force, remove_orphan_batch_worktrees_with,
};
use cleanup::is_other_branch_holder;
pub use cleanup::{
    cleanup_missing_worktree, cleanup_verified_merged_task, delete_snapshot_refs,
    is_operation_in_progress, remove_verified_merged,
};
pub use reattach::{attach_existing_branch, attach_existing_branch_with_warning};

pub use aaf_domain::engine_contracts::BRANCH_PREFIX;
pub const SNAPSHOT_REF_PREFIX: &str = "refs/aaf/snapshots/";
pub(super) const BATCH_PREFIX: &str = "batch-";
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

/// Fails with [`GitError::Invalid`] when `tree`'s `HEAD` is detached or does
/// not point at `refs/heads/{branch}`.
pub(crate) fn ensure_on_branch(tree: &Path, branch: &str) -> Result<(), GitError> {
    let expected = format!("refs/heads/{branch}");
    match git(tree, &["symbolic-ref", "-q", "HEAD"]) {
        Ok(head) if head == expected => Ok(()),
        Ok(head) => Err(GitError::Invalid(format!(
            "{} is not on branch {branch}: HEAD is {head}",
            tree.display()
        ))),
        Err(GitError::Command { code, stderr, .. }) => {
            let detail = if stderr.is_empty() {
                format!("git symbolic-ref exited with {code:?}")
            } else {
                format!("git symbolic-ref exited with {code:?}: {stderr}")
            };
            Err(GitError::Invalid(format!(
                "{} is not on branch {branch}: {detail}",
                tree.display()
            )))
        }
        Err(err) => Err(err),
    }
}

pub(crate) use super::reflog_guard::ensure_plain_reflogs;

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
    ensure_plain_reflogs(repo_root)?;
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
    if !is_non_symlink_dir(&worktree.path) {
        return Err(GitError::Invalid(format!(
            "{} is not a worktree directory of branch {}",
            worktree.path.display(),
            worktree.branch
        )));
    }
    let wanted = fs::canonicalize(&worktree.path)?;
    let entries = list(repo_root)?;
    if entries
        .first()
        .is_some_and(|main| fs::canonicalize(&main.path).is_ok_and(|p| p == wanted))
    {
        return Err(GitError::Invalid(format!(
            "{} is the main worktree, refusing removal",
            worktree.path.display()
        )));
    }
    let registered = entries.iter().skip(1).any(|entry| {
        entry.branch.as_deref() == Some(worktree.branch.as_str())
            && is_non_symlink_dir(&entry.path)
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

/// True when `path` is an actual directory on disk and NOT a symlink.
pub(crate) fn is_non_symlink_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
}

/// Returns the task's registered worktree and its `HEAD` oid only when the
/// worktree directory actually exists on disk as a real (non-symlink) directory.
/// Checks `worktrees_dir` first, then falls back to an AAF-managed worktree
/// matching `<repo_id>/<task_id>` outside the main checkout (e.g. when
/// `AAF_WORKTREE_DIR` changed between runs). Never adopts symlinks, main
/// checkouts, or foreign checkouts.
pub fn existing_registered(
    repo_root: &Path,
    worktrees_dir: &Path,
    task_id: &str,
) -> Result<Option<(TaskWorktree, String)>, GitError> {
    let expected = worktree_path(repo_root, worktrees_dir, task_id)?;
    let branch = format!("{BRANCH_PREFIX}{task_id}");
    let entries = list(repo_root)?;
    if is_non_symlink_dir(&expected)
        && let Ok(resolved_expected) = fs::canonicalize(&expected)
        && entries.iter().any(|entry| {
            entry.branch.as_deref() == Some(branch.as_str())
                && is_non_symlink_dir(&entry.path)
                && fs::canonicalize(&entry.path).is_ok_and(|p| p == resolved_expected)
                && is_aaf_managed_worktree_path(&entry.path, &expected, &entries)
        })
    {
        let head = git(&expected, &["rev-parse", "HEAD"])?;
        return Ok(Some((
            TaskWorktree {
                path: expected,
                branch,
            },
            head,
        )));
    }
    if let Some(entry) = entries.iter().find(|entry| {
        entry.branch.as_deref() == Some(branch.as_str())
            && is_non_symlink_dir(&entry.path)
            && is_aaf_managed_worktree_path(&entry.path, &expected, &entries)
    }) {
        let head = git(&entry.path, &["rev-parse", "HEAD"])?;
        return Ok(Some((
            TaskWorktree {
                path: entry.path.clone(),
                branch,
            },
            head,
        )));
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

fn path_eq_or_under(path: &Path, ancestor: &Path) -> bool {
    path == ancestor || path.starts_with(ancestor) || under_ignoring_case(path, ancestor)
}

/// APFS is case-insensitive by default: `/Users/x/Repo` and `/users/x/repo`
/// are one directory there.
#[cfg(target_os = "macos")]
fn under_ignoring_case(path: &Path, ancestor: &Path) -> bool {
    let p_comps: Vec<_> = path.components().collect();
    let a_comps: Vec<_> = ancestor.components().collect();
    a_comps.len() <= p_comps.len()
        && a_comps.iter().zip(&p_comps).all(|(a, p)| {
            a.as_os_str()
                .to_string_lossy()
                .eq_ignore_ascii_case(&p.as_os_str().to_string_lossy())
        })
}

#[cfg(not(target_os = "macos"))]
fn under_ignoring_case(_path: &Path, _ancestor: &Path) -> bool {
    false
}

/// True when `entry_path` is an AAF-managed worktree for the same `(repo_id, task_id)`
/// as `expected`: never the main checkout, never inside any registered worktree,
/// and either equal to `expected` or matching its exact trailing `<repo_id>/<task_id>`
/// components (so worktrees created under a previous `AAF_WORKTREE_DIR` remain
/// recognized while human checkouts stay protected as foreign).
pub(crate) fn is_aaf_managed_worktree_path(
    entry_path: &Path,
    expected: &Path,
    registered: &[WorktreeEntry],
) -> bool {
    let entry_res = resolved(entry_path);
    if let Some(main) = registered.first() {
        let main_res = resolved(&main.path);
        if path_eq_or_under(&entry_res, &main_res) {
            return false;
        }
    }
    if registered.iter().skip(1).any(|other| {
        let other_res = resolved(&other.path);
        other_res != entry_res && path_eq_or_under(&entry_res, &other_res)
    }) {
        return false;
    }
    let expected_res = resolved_leaf_no_follow(expected);
    if entry_res == expected_res {
        return true;
    }
    match (
        entry_res.file_name(),
        expected_res.file_name(),
        entry_res.parent().and_then(Path::file_name),
        expected_res.parent().and_then(Path::file_name),
    ) {
        (Some(ef), Some(xf), Some(ep), Some(xp)) => ef == xf && ep == xp,
        _ => false,
    }
}

/// Finds the registered worktree entry for a task (by branch, exact path, or
/// relocated AAF-managed `<repo_id>/<task_id>` path, including detached HEAD).
pub(crate) fn find_registered_for_task<'a>(
    entries: &'a [WorktreeEntry],
    expected: &Path,
    branch: &str,
) -> Option<&'a WorktreeEntry> {
    let expected_res = resolved(expected);
    entries
        .iter()
        .find(|e| e.branch.as_deref() == Some(branch))
        .or_else(|| entries.iter().find(|e| resolved(&e.path) == expected_res))
        .or_else(|| {
            entries
                .iter()
                .find(|e| is_aaf_managed_worktree_path(&e.path, expected, entries))
        })
}

/// `"<id>: branch is checked out in <where>: kept"` when `entry` (the
/// registered tree holding task `id`'s branch) is not the factory's own
/// worktree at `expected` (or an AAF-managed `<repo_id>/<id>` worktree from a
/// previous `AAF_WORKTREE_DIR`); `None` when it is. git lists the main worktree first.
pub(crate) fn foreign_checkout(
    id: &str,
    entry: &WorktreeEntry,
    registered: &[WorktreeEntry],
    expected: &Path,
) -> Option<String> {
    if registered
        .first()
        .is_some_and(|main| resolved(&main.path) == resolved(&entry.path))
    {
        return Some(format!(
            "{id}: branch is checked out in the main worktree: kept"
        ));
    }
    (!is_aaf_managed_worktree_path(&entry.path, expected, registered)).then(|| {
        format!(
            "{id}: branch is checked out in {}, not its aaf worktree: kept",
            entry.path.display()
        )
    })
}

/// Resolves `path`'s ancestors without following a symlink at `path` itself,
/// so a symlink planted at `<worktrees_dir>/<repo_id>/<task_id>` pointing to a
/// foreign checkout cannot spoof `expected_res == entry_res`.
fn resolved_leaf_no_follow(path: &Path) -> PathBuf {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => resolved(parent).join(name),
        _ => resolved(path),
    }
}

/// Resolves `path` through its deepest existing canonical ancestor so missing
/// paths under symlinked roots (e.g. `/tmp` -> `/private/tmp`) still match.
pub(super) fn resolved(path: &Path) -> PathBuf {
    if let Ok(real) = fs::canonicalize(path) {
        return real;
    }
    let mut tail = Vec::new();
    let mut cur = path;
    while let Some(name) = cur.file_name() {
        tail.push(name.to_os_string());
        let Some(parent) = cur.parent() else {
            break;
        };
        if let Ok(real_parent) = fs::canonicalize(parent) {
            return tail
                .into_iter()
                .rev()
                .fold(real_parent, |acc, seg| acc.join(seg));
        }
        cur = parent;
    }
    path.to_path_buf()
}

pub use super::worktree_inspect::{
    active_in_progress_branches, active_in_progress_branches_for, list, prune, prune_with_expire,
    removal_refusal,
};
