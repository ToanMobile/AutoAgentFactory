//! Safe orphan `aaf/batch-*` worktree and branch sweeping.
//!
//! Protects live PID-tagged batches (`batch-<pid>-<8hex>`), main/foreign
//! checkouts, `git worktree lock`ed trees, trees containing `cwd`, and
//! worktrees/branches with active git operations (rebase/merge/cherry-pick/revert).

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub use crate::process::process_alive;

use super::super::cmd::{git, git_raw};
use super::super::{GitError, path_str};
use super::cleanup::{
    delete_branch_ignore_missing, delete_snapshot_refs, is_operation_in_progress,
};
use super::{
    BATCH_PREFIX, BRANCH_PREFIX, WorktreeEntry, active_in_progress_branches_for, foreign_checkout,
    list, prune, removal_refusal, resolved, validate_task_id,
};

/// Outcome of [`remove_orphan_batch_worktrees`] and
/// [`remove_orphan_batch_worktrees_with`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrphanSweep {
    /// Removed (or removable, on a dry run) batch names.
    pub removed: Vec<String>,
    /// Per-entry warnings for orphans kept or failed to remove.
    pub warnings: Vec<String>,
}

/// Alias for [`OrphanSweep`] for callers naming the batch sweep report explicitly.
pub type OrphanBatchSweep = OrphanSweep;

/// Extracts the owning PID from a PID-tagged batch id (`batch-<pid>-<8hex>`).
/// Returns `None` for legacy non-PID batch ids such as `batch-<12hex>` or
/// `batch-orphan1`.
pub fn parse_batch_pid(id: &str) -> Option<u32> {
    let rest = id.strip_prefix(BATCH_PREFIX)?;
    let (pid_part, hex_part) = rest.split_once('-')?;
    if pid_part.is_empty()
        || !pid_part.bytes().all(|b| b.is_ascii_digit())
        || hex_part.len() != 8
        || !hex_part.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    pid_part.parse::<u32>().ok()
}

enum CwdGuard {
    None,
    Resolved(PathBuf),
    Unresolvable(String),
}

impl CwdGuard {
    fn from_option(cwd: Option<&Path>) -> Self {
        match cwd {
            None => Self::None,
            Some(path) => match fs::canonicalize(path) {
                Ok(canon) => Self::Resolved(canon),
                Err(err) => {
                    Self::Unresolvable(format!("cannot canonicalize cwd {}: {err}", path.display()))
                }
            },
        }
    }

    fn blocks_worktree(&self, wt: &Path) -> Option<String> {
        match self {
            Self::None => None,
            Self::Unresolvable(reason) => Some(reason.clone()),
            Self::Resolved(canon_cwd) => match fs::canonicalize(wt) {
                Ok(canon_wt) if canon_cwd.starts_with(&canon_wt) => Some(format!(
                    "cwd {} is inside worktree {}",
                    canon_cwd.display(),
                    wt.display()
                )),
                Ok(_) => None,
                Err(err) if wt.exists() => Some(format!(
                    "cannot canonicalize worktree {}: {err}",
                    wt.display()
                )),
                Err(_) => {
                    let res_wt = resolved(wt);
                    canon_cwd.starts_with(&res_wt).then(|| {
                        format!(
                            "cwd {} is inside worktree {}",
                            canon_cwd.display(),
                            wt.display()
                        )
                    })
                }
            },
        }
    }
}

/// Makes directories and regular files under `root` owner-writable without
/// ever following symlinks, so `git worktree remove --force` or
/// `fs::remove_dir_all` can clean read-only files in a dead dirty batch tree.
fn make_tree_owner_writable(root: &Path) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        let mode = meta.permissions().mode();
        if meta.is_dir() {
            if mode & 0o700 != 0o700 {
                let _ = fs::set_permissions(&path, fs::Permissions::from_mode(mode | 0o700));
            }
            if let Ok(entries) = fs::read_dir(&path) {
                for entry in entries.flatten() {
                    stack.push(entry.path());
                }
            }
        } else if mode & 0o200 == 0 {
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(mode | 0o200));
        }
    }
}

fn remove_dir_all_safe(path: &Path, force: bool) -> std::io::Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(_) if force => {
            make_tree_owner_writable(path);
            fs::remove_dir_all(path)
        }
        Err(err) => Err(err),
    }
}

/// Fail-closed dirty check: any error or non-zero `git status` counts as dirty.
fn is_worktree_dirty(wt: &Path) -> bool {
    match git_raw(wt, &["status", "--porcelain"]) {
        Ok(out) if out.success() => !out.stdout.trim().is_empty(),
        _ => true,
    }
}

fn collect_batch_candidates(
    repo_root: &Path,
    repo_wt_dir: &Path,
    registered: &[WorktreeEntry],
    active_task_ids: &HashSet<String>,
    batch_short_branches: Option<&[String]>,
) -> Result<BTreeSet<String>, GitError> {
    let mut candidates = BTreeSet::new();
    if let Ok(entries) = fs::read_dir(repo_wt_dir) {
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
    let listed_branches: Vec<String>;
    let short_branches: &[String] = match batch_short_branches {
        Some(pre) => pre,
        None => {
            let raw = git(
                repo_root,
                &[
                    "for-each-ref",
                    "--format=%(refname:short)",
                    "refs/heads/aaf/batch-*",
                ],
            )?;
            listed_branches = raw
                .lines()
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect();
            &listed_branches
        }
    };
    for short in short_branches {
        if let Some(id) = short.strip_prefix(BRANCH_PREFIX)
            && id.starts_with(BATCH_PREFIX)
            && validate_task_id(id).is_ok()
            && !active_task_ids.contains(id)
        {
            candidates.insert(id.to_string());
        }
    }
    let resolved_wt_dir = resolved(repo_wt_dir);
    for entry in registered {
        if let Some(id) = entry.task_id()
            && id.starts_with(BATCH_PREFIX)
            && !active_task_ids.contains(id)
        {
            candidates.insert(id.to_string());
        } else if let Some(parent) = resolved(&entry.path).parent()
            && parent == resolved_wt_dir
            && let Some(id) = entry.path.file_name().and_then(|n| n.to_str())
            && id.starts_with(BATCH_PREFIX)
            && validate_task_id(id).is_ok()
            && !active_task_ids.contains(id)
        {
            candidates.insert(id.to_string());
        }
    }
    Ok(candidates)
}

/// Inspects `<worktrees_root>/<repo_id>/` and local `aaf/batch-*` refs for
/// orphaned integration worktrees/branches not present in `active_task_ids`.
///
/// Backward-compatible wrapper calling [`remove_orphan_batch_worktrees_with`]
/// with `force = false` and `cwd = None`.
pub fn remove_orphan_batch_worktrees(
    repo_root: &Path,
    worktrees_root: &Path,
    repo_id: &str,
    active_task_ids: &HashSet<String>,
    dry_run: bool,
) -> Result<OrphanSweep, GitError> {
    remove_orphan_batch_worktrees_with(
        repo_root,
        worktrees_root,
        repo_id,
        active_task_ids,
        dry_run,
        false,
        None,
    )
}

/// Sweeps orphaned `aaf/batch-*` worktrees and branches with fail-closed guards.
///
/// - Skips any PID-tagged `batch-<pid>-<8hex>` whose owning process is alive or
///   equals the current PID (even when `force == true`).
/// - Matches registered worktrees by both branch and canonical path so detached
///   locked worktrees cannot bypass `entry.locked`.
/// - Skips the main worktree, foreign checkouts, `locked` worktrees, trees
///   containing `cwd` (fail-closed if `cwd` cannot be canonicalized), and
///   trees/branches with an active git operation in progress.
/// - Dirty worktrees are kept unless `force == true` (and `force` still never
///   passes `-f -f` or removes locked/live/in-progress/`cwd` trees).
/// - Per-entry errors are collected into [`OrphanSweep::warnings`] instead of
///   aborting the rest of the sweep.
#[allow(clippy::too_many_arguments)]
pub fn remove_orphan_batch_worktrees_with(
    repo_root: &Path,
    worktrees_root: &Path,
    repo_id: &str,
    active_task_ids: &HashSet<String>,
    dry_run: bool,
    force: bool,
    cwd: Option<&Path>,
) -> Result<OrphanSweep, GitError> {
    remove_orphan_batch_worktrees_inner(
        repo_root,
        worktrees_root,
        repo_id,
        active_task_ids,
        dry_run,
        force,
        force,
        cwd,
    )
}

/// Offline startup-sweep variant: uses `force = true` **only** for dead
/// PID-tagged batches (`batch-<dead_pid>-<8hex>`), keeping `force = false` for
/// legacy non-PID `batch-*` worktrees so dirty/uninspected legacy trees are
/// never forcibly discarded at startup.
pub fn remove_orphan_batch_worktrees_dead_pid_force(
    repo_root: &Path,
    worktrees_root: &Path,
    repo_id: &str,
    active_task_ids: &HashSet<String>,
    dry_run: bool,
    cwd: Option<&Path>,
) -> Result<OrphanSweep, GitError> {
    remove_orphan_batch_worktrees_inner(
        repo_root,
        worktrees_root,
        repo_id,
        active_task_ids,
        dry_run,
        false,
        true,
        cwd,
    )
}

#[allow(clippy::too_many_arguments)]
fn remove_orphan_batch_worktrees_inner(
    repo_root: &Path,
    worktrees_root: &Path,
    repo_id: &str,
    active_task_ids: &HashSet<String>,
    dry_run: bool,
    force_all: bool,
    force_dead_pid: bool,
    cwd: Option<&Path>,
) -> Result<OrphanSweep, GitError> {
    let registered = list(repo_root)?;
    let active_ops = active_in_progress_branches_for(&registered);
    remove_orphan_batch_worktrees_prepared(
        repo_root,
        worktrees_root,
        repo_id,
        &registered,
        &active_ops,
        None,
        active_task_ids,
        dry_run,
        force_all,
        force_dead_pid,
        cwd,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn remove_orphan_batch_worktrees_prepared(
    repo_root: &Path,
    worktrees_root: &Path,
    repo_id: &str,
    registered: &[WorktreeEntry],
    active_ops: &HashSet<String>,
    batch_short_branches: Option<&[String]>,
    active_task_ids: &HashSet<String>,
    dry_run: bool,
    force_all: bool,
    force_dead_pid: bool,
    cwd: Option<&Path>,
) -> Result<OrphanSweep, GitError> {
    let repo_wt_dir = worktrees_root.join(repo_id);
    let candidates = collect_batch_candidates(
        repo_root,
        &repo_wt_dir,
        registered,
        active_task_ids,
        batch_short_branches,
    )?;
    let cwd_guard = CwdGuard::from_option(cwd);
    let own_pid = std::process::id();

    let mut sweep = OrphanSweep::default();
    for id in candidates {
        let batch_pid = parse_batch_pid(&id);
        if let Some(pid) = batch_pid
            && (pid == own_pid || process_alive(pid))
        {
            continue;
        }
        let force = force_all || (force_dead_pid && batch_pid.is_some());
        let branch = format!("{BRANCH_PREFIX}{id}");
        if active_ops.contains(&branch) {
            sweep
                .warnings
                .push(format!("{id}: git operation in progress on {branch}: kept"));
            continue;
        }
        let expected_path = repo_wt_dir.join(&id);
        let expected_res = resolved(&expected_path);
        let branch_entry = registered
            .iter()
            .find(|e| e.branch.as_deref() == Some(branch.as_str()));
        if let Some(entry) = branch_entry
            && let Some(warning) = foreign_checkout(&id, entry, registered, &expected_path)
        {
            sweep.warnings.push(warning);
            continue;
        }
        let path_entry = registered
            .iter()
            .find(|e| resolved(&e.path) == expected_res);
        let reg_entry = branch_entry.or(path_entry);

        let keep = match reg_entry {
            Some(entry) => !clean_registered_batch(
                repo_root,
                &id,
                entry,
                registered,
                &expected_path,
                &cwd_guard,
                dry_run,
                force,
                &mut sweep.warnings,
            ),
            None if expected_path.exists() || fs::symlink_metadata(&expected_path).is_ok() => {
                !clean_unregistered_batch(
                    &id,
                    &expected_path,
                    &cwd_guard,
                    dry_run,
                    force,
                    &mut sweep.warnings,
                )
            }
            None => false,
        };
        if keep {
            continue;
        }

        if !dry_run {
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

#[allow(clippy::too_many_arguments)]
fn clean_registered_batch(
    repo_root: &Path,
    id: &str,
    entry: &WorktreeEntry,
    registered: &[WorktreeEntry],
    expected_path: &Path,
    cwd_guard: &CwdGuard,
    dry_run: bool,
    force: bool,
    warnings: &mut Vec<String>,
) -> bool {
    if let Some(warning) = foreign_checkout(id, entry, registered, expected_path) {
        warnings.push(warning);
        return false;
    }
    if entry.locked {
        if !entry.path.exists() {
            warnings.push(format!(
                "{id}: worktree is locked and its directory is missing: kept"
            ));
        } else if dry_run {
            warnings.push(format!(
                "{id}: worktree remove: would be refused: cannot remove a locked working tree"
            ));
        } else {
            warnings.push(format!(
                "{id}: worktree remove: cannot remove a locked working tree"
            ));
        }
        return false;
    }
    if let Some(reason) = cwd_guard.blocks_worktree(&entry.path) {
        warnings.push(format!("{id}: {reason}: kept"));
        return false;
    }
    if !entry.path.exists() {
        if !dry_run && let Err(err) = prune(repo_root) {
            warnings.push(format!("{id}: worktree prune: {err}"));
            return false;
        }
        return true;
    }
    match is_operation_in_progress(&entry.path) {
        Ok(true) => {
            warnings.push(format!("{id}: git operation in progress: kept"));
            return false;
        }
        Ok(false) => {}
        Err(err) => {
            warnings.push(format!("{id}: operation check failed: {err}"));
            return false;
        }
    }
    if is_worktree_dirty(&entry.path) && !force {
        return false;
    }
    let wt_str = match path_str(&entry.path) {
        Ok(p) => p,
        Err(err) => {
            warnings.push(format!("{id}: worktree remove: {err}"));
            return false;
        }
    };
    if dry_run {
        if !force {
            match removal_refusal(&entry.path) {
                Ok(Some(reason)) => {
                    warnings.push(format!("{id}: worktree remove: would be refused: {reason}"));
                    return false;
                }
                Ok(None) => {}
                Err(err) => {
                    warnings.push(format!("{id}: worktree remove: {err}"));
                    return false;
                }
            }
        }
        return true;
    }
    if force {
        make_tree_owner_writable(&entry.path);
    }
    let remove_args: &[&str] = if force {
        &["worktree", "remove", "--force", wt_str]
    } else {
        &["worktree", "remove", wt_str]
    };
    let out = match git_raw(repo_root, remove_args) {
        Ok(out) => out,
        Err(err) => {
            warnings.push(format!("{id}: worktree remove: {err}"));
            return false;
        }
    };
    if !out.success() {
        warnings.push(format!("{id}: worktree remove: {}", out.stderr.trim()));
        return false;
    }
    true
}

fn clean_unregistered_batch(
    id: &str,
    expected_path: &Path,
    cwd_guard: &CwdGuard,
    dry_run: bool,
    force: bool,
    warnings: &mut Vec<String>,
) -> bool {
    if let Some(reason) = cwd_guard.blocks_worktree(expected_path) {
        warnings.push(format!("{id}: {reason}: kept"));
        return false;
    }
    let Ok(meta) = fs::symlink_metadata(expected_path) else {
        return false;
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return false;
    }
    if expected_path.join(".git").exists() {
        match is_operation_in_progress(expected_path) {
            Ok(true) => {
                warnings.push(format!("{id}: git operation in progress: kept"));
                return false;
            }
            Ok(false) => {}
            Err(_) if !force => return false,
            Err(_) => {}
        }
        if is_worktree_dirty(expected_path) && !force {
            return false;
        }
        if !dry_run && let Err(err) = remove_dir_all_safe(expected_path, force) {
            warnings.push(format!("{id}: remove dir: {err}"));
            return false;
        }
        return true;
    }
    let is_empty = match fs::read_dir(expected_path) {
        Ok(mut entries) => entries.next().transpose().ok().flatten().is_none(),
        Err(err) => {
            warnings.push(format!("{id}: read dir: {err}"));
            return false;
        }
    };
    if !is_empty && !force {
        return false;
    }
    if !dry_run {
        let res = if is_empty {
            fs::remove_dir(expected_path)
        } else {
            remove_dir_all_safe(expected_path, true)
        };
        if let Err(err) = res {
            warnings.push(format!("{id}: remove dir: {err}"));
            return false;
        }
    }
    true
}
