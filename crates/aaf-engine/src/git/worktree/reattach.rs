//! Safe re-attachment of an existing local `aaf/<task_id>` branch when its
//! worktree directory is absent on disk.

use std::fs;
use std::path::{Path, PathBuf};

use super::super::cmd::{git, git_raw};
use super::super::{GitError, path_str};
use super::{
    BRANCH_PREFIX, TaskWorktree, is_aaf_managed_worktree_path, list, prune, resolved, submodules,
    worktree_path,
};

/// Attaches a fresh worktree under `worktrees_dir` to an already-existing local
/// task branch `aaf/<task_id>` when no live worktree currently holds it.
///
/// Returns `Ok(None)` when `refs/heads/aaf/<task_id>` does not exist.
/// Fails closed if `path` already exists unregistered, if `aaf/<task_id>` is
/// checked out in another existing worktree (main checkout or foreign tree), if
/// a missing worktree entry is foreign or `locked`, or if a detached/locked
/// entry occupies `path`. Uses `-c core.hooksPath=/dev/null` and only initializes
/// submodules when `.gitmodules` matches trusted `base_sha`.
pub fn attach_existing_branch(
    repo_root: &Path,
    worktrees_dir: &Path,
    task_id: &str,
    base_sha: Option<&str>,
) -> Result<Option<TaskWorktree>, GitError> {
    let Some((tree, warning)) =
        attach_existing_branch_with_warning(repo_root, worktrees_dir, task_id, base_sha)?
    else {
        return Ok(None);
    };
    if let Some(warning) = warning {
        eprintln!("aaf: warning: {warning}");
    }
    Ok(Some(tree))
}

/// [`attach_existing_branch`], returning any submodule warning instead of printing it.
pub fn attach_existing_branch_with_warning(
    repo_root: &Path,
    worktrees_dir: &Path,
    task_id: &str,
    base_sha: Option<&str>,
) -> Result<Option<(TaskWorktree, Option<String>)>, GitError> {
    let path = worktree_path(repo_root, worktrees_dir, task_id)?;
    let branch = format!("{BRANCH_PREFIX}{task_id}");
    let branch_ref = format!("refs/heads/{branch}");
    if !git_raw(repo_root, &["show-ref", "--verify", "--quiet", &branch_ref])?.success() {
        return Ok(None);
    }
    if path.exists() || fs::symlink_metadata(&path).is_ok() {
        return Err(GitError::AlreadyExists(path.display().to_string()));
    }
    let entries = list(repo_root)?;
    let expected_res = resolved(&path);
    let mut stale_paths = Vec::new();
    for entry in &entries {
        let holds_branch = entry.branch.as_deref() == Some(branch.as_str());
        let occupies_path = resolved(&entry.path) == expected_res;
        if !holds_branch && !occupies_path {
            continue;
        }
        if entry.path.exists() || fs::symlink_metadata(&entry.path).is_ok() {
            return Err(GitError::Invalid(format!(
                "branch {branch} is already checked out at {}",
                entry.path.display()
            )));
        }
        if entry.locked {
            return Err(GitError::Invalid(format!(
                "worktree for branch {branch} is locked at {}",
                entry.path.display()
            )));
        }
        if holds_branch && !is_aaf_managed_worktree_path(&entry.path, &path, &entries) {
            return Err(GitError::Invalid(format!(
                "branch {branch} is registered to foreign worktree {}",
                entry.path.display()
            )));
        }
        stale_paths.push(entry.path.clone());
    }
    for stale in &stale_paths {
        prune_stale_entry_admin_dir(repo_root, stale)?;
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    git(
        repo_root,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "worktree",
            "add",
            path_str(&path)?,
            &branch,
        ],
    )?;
    let warning = if gitmodules_trusted(repo_root, &branch_ref, base_sha) {
        submodules::init(repo_root, &path)
    } else {
        Some("skipped submodule init on reattached branch: .gitmodules differs from base".into())
    };
    Ok(Some((TaskWorktree { path, branch }, warning)))
}

fn gitmodules_trusted(repo_root: &Path, branch_ref: &str, base_sha: Option<&str>) -> bool {
    let branch_spec = format!("{branch_ref}:.gitmodules");
    let branch_blob = git_raw(
        repo_root,
        &["rev-parse", "--verify", "--quiet", &branch_spec],
    )
    .ok()
    .filter(|o| o.success())
    .map(|o| o.stdout.trim().to_string());
    let Some(branch_oid) = branch_blob else {
        return true;
    };
    let Some(base) = base_sha else {
        return false;
    };
    let base_spec = format!("{base}:.gitmodules");
    let base_blob = git_raw(repo_root, &["rev-parse", "--verify", "--quiet", &base_spec])
        .ok()
        .filter(|o| o.success())
        .map(|o| o.stdout.trim().to_string());
    base_blob.as_deref() == Some(branch_oid.as_str())
}

/// Removes ONLY the unlocked `.git/worktrees/<name>` admin directory whose
/// `gitdir` points to `missing_wt/.git`, avoiding a global `git worktree prune`
/// that could race with concurrent processes.
fn prune_stale_entry_admin_dir(repo_root: &Path, missing_wt: &Path) -> Result<(), GitError> {
    let common = PathBuf::from(
        git(
            repo_root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?
        .trim(),
    );
    let wt_admins = common.join("worktrees");
    if !wt_admins.is_dir() {
        return Ok(());
    }
    let want = resolved(&missing_wt.join(".git"));
    let mut removed = false;
    for dir_entry in fs::read_dir(&wt_admins)? {
        let admin = dir_entry?.path();
        if !admin.is_dir() || admin.join("locked").exists() {
            continue;
        }
        let Ok(raw) = fs::read_to_string(admin.join("gitdir")) else {
            continue;
        };
        let target = PathBuf::from(raw.trim());
        let abs = if target.is_absolute() {
            target
        } else {
            admin.join(target)
        };
        if resolved(&abs) == want {
            fs::remove_dir_all(&admin)?;
            removed = true;
        }
    }
    if !removed {
        prune(repo_root)?;
    }
    Ok(())
}
