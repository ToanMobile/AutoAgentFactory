//! Safe worktree, branch, and snapshot cleanup helpers (normal merged cleanup,
//! missing-directory cleanup, and verified squash/rebase merge cleanup).

use std::fs;
use std::path::{Path, PathBuf};

use super::super::cmd::{failure, git, git_raw};
use super::super::{GitError, path_str};
use super::{
    BRANCH_PREFIX, SNAPSHOT_REF_PREFIX, SUBMODULES_REFUSAL, TaskWorktree, ensure_clean, list,
    prune, removal_refusal, resolved, validate_task_id, worktree_path,
};

/// True when `wt` has an active git rebase, merge, cherry-pick, or revert.
pub fn is_operation_in_progress(wt: &Path) -> Result<bool, GitError> {
    let git_dir = PathBuf::from(git(wt, &["rev-parse", "--absolute-git-dir"])?.trim());
    Ok([
        "rebase-merge",
        "rebase-apply",
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
    ]
    .iter()
    .any(|marker| git_dir.join(marker).exists()))
}

/// True when `refname` (from `git for-each-ref --contains`) proves `branch` is
/// contained in a *different* local or remote-tracking branch.
///
/// Rejects the branch itself (`refs/heads/<branch>`), any self-tracking remote
/// ref (`refs/remotes/<remote>/<branch>`), symbolic remote `HEAD` refs
/// (`refs/remotes/<remote>/HEAD`), and anything outside `refs/heads/` or
/// `refs/remotes/`.
pub(super) fn is_other_branch_holder(refname: &str, branch: &str, branch_ref: &str) -> bool {
    if refname == branch_ref {
        return false;
    }
    if refname.starts_with("refs/heads/") {
        return true;
    }
    let Some(rest) = refname.strip_prefix("refs/remotes/") else {
        return false;
    };
    let Some((remote, tail)) = rest.split_once('/') else {
        return false;
    };
    !remote.is_empty() && tail != branch && tail != "HEAD"
}

fn is_valid_oid(oid: &str) -> bool {
    (40..=64).contains(&oid.len()) && oid.bytes().all(|b| b.is_ascii_hexdigit())
}

pub(super) fn delete_branch_ignore_missing(repo_root: &Path, branch: &str) -> Result<(), GitError> {
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

/// Ensures no foreign/detached/locked worktree still holds `branch` or sits at
/// `expected_path` when the task worktree directory is absent on disk.
fn ensure_no_conflicting_worktree(
    repo_root: &Path,
    expected_path: &Path,
    branch: &str,
) -> Result<(), GitError> {
    let expected_res = resolved(expected_path);
    for entry in list(repo_root)? {
        if entry.branch.as_deref() == Some(branch) {
            if entry.path.exists() {
                return Err(GitError::Invalid(format!(
                    "branch {branch} is still checked out at {}",
                    entry.path.display()
                )));
            }
            if entry.locked {
                return Err(GitError::Invalid(format!(
                    "worktree for branch {branch} is locked at {}",
                    entry.path.display()
                )));
            }
        } else if resolved(&entry.path) == expected_res && (entry.path.exists() || entry.locked) {
            return Err(GitError::Invalid(format!(
                "worktree at {} is detached or locked",
                entry.path.display()
            )));
        }
    }
    Ok(())
}

fn branch_exists(repo_root: &Path, branch_ref: &str) -> Result<bool, GitError> {
    Ok(git_raw(repo_root, &["show-ref", "--verify", "--quiet", branch_ref])?.success())
}

/// Cleans up branch and snapshot refs for `task_id` when its worktree directory
/// is already absent (`RealGit::existing` returned `None`).
///
/// Fails closed if `expected_path` still exists on disk (e.g. detached HEAD or
/// unregistered directory), if the branch is checked out elsewhere or locked,
/// or if the branch still exists and is not an ancestor of `start_ref`.
/// Snapshot refs are deleted only after `branch -D` succeeds (or when the
/// branch is already gone).
pub fn cleanup_missing_worktree(
    repo_root: &Path,
    worktrees_dir: &Path,
    task_id: &str,
    start_ref: &str,
) -> Result<(), GitError> {
    validate_task_id(task_id)?;
    let expected_path = worktree_path(repo_root, worktrees_dir, task_id)?;
    let branch = format!("{BRANCH_PREFIX}{task_id}");
    if expected_path.exists() || fs::symlink_metadata(&expected_path).is_ok() {
        return Err(GitError::Invalid(format!(
            "worktree path {} exists but is not registered on branch {branch}",
            expected_path.display()
        )));
    }
    ensure_no_conflicting_worktree(repo_root, &expected_path, &branch)?;
    let branch_ref = format!("refs/heads/{branch}");
    if !branch_exists(repo_root, &branch_ref)? {
        prune(repo_root)?;
        delete_snapshot_refs(repo_root, task_id)?;
        return Ok(());
    }
    let is_ancestor = git_raw(
        repo_root,
        &["merge-base", "--is-ancestor", &branch_ref, start_ref],
    )?
    .code
        == Some(0);
    if !is_ancestor {
        return Err(GitError::Unmerged(branch));
    }
    prune(repo_root)?;
    git(repo_root, &["branch", "-D", &branch])?;
    delete_snapshot_refs(repo_root, task_id)?;
    Ok(())
}

/// Removes a worktree and branch whose PR was externally verified as
/// squash-merged at `expected_head_oid`.
///
/// Fails closed: refuses dirty worktrees (`GitError::Dirty`), refuses active
/// git operations (rebase/merge/cherry-pick/revert), refuses HEAD mismatches
/// against `expected_head_oid`, never passes `--force` to `git worktree remove`,
/// deletes the branch (skipping `--contains`), and deletes
/// `refs/aaf/snapshots/<task_id>/*` only after branch deletion succeeds.
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
    let branch_ref = format!("refs/heads/{}", worktree.branch);
    if worktree.path.exists() {
        let wanted = fs::canonicalize(&worktree.path)?;
        let reg_entry = list(repo_root)?.into_iter().find(|entry| {
            entry.branch.as_deref() == Some(worktree.branch.as_str())
                && fs::canonicalize(&entry.path).is_ok_and(|p| p == wanted)
        });
        let Some(entry) = reg_entry else {
            return Err(GitError::Invalid(format!(
                "{} is not a worktree of branch {}",
                worktree.path.display(),
                worktree.branch
            )));
        };
        if entry.locked {
            return Err(GitError::Invalid(format!(
                "worktree {} is locked",
                worktree.path.display()
            )));
        }
        if is_operation_in_progress(&worktree.path)? {
            return Err(GitError::Invalid(format!(
                "git operation in progress in {}",
                worktree.path.display()
            )));
        }
        ensure_clean(&worktree.path)?;
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
        ensure_no_conflicting_worktree(repo_root, &worktree.path, &worktree.branch)?;
        prune(repo_root)?;
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

fn delete_branch_if_at_head(
    repo_root: &Path,
    branch: &str,
    expected_head: &str,
) -> Result<(), GitError> {
    let branch_ref = format!("refs/heads/{branch}");
    let rev = git_raw(
        repo_root,
        &["rev-parse", "--verify", "--quiet", &branch_ref],
    )?;
    if !rev.success() {
        return Ok(());
    }
    let tip = rev.stdout.trim();
    if !tip.eq_ignore_ascii_case(expected_head) {
        return Err(GitError::Unmerged(format!(
            "{branch} (tip {tip} != expected {expected_head})"
        )));
    }
    let expected_lower = expected_head.to_ascii_lowercase();
    let del = git_raw(
        repo_root,
        &["update-ref", "-d", &branch_ref, &expected_lower],
    )?;
    if !del.success() && branch_exists(repo_root, &branch_ref)? {
        return Err(GitError::Unmerged(format!(
            "{branch} (ref changed concurrently: {})",
            del.stderr.trim()
        )));
    }
    Ok(())
}

/// Cleans a task whose PR has already been verified as merged (merge, squash, or
/// rebase) at `expected_head` without requiring `for-each-ref --contains` ancestry.
///
/// Refuses invalid/empty `expected_head`, dirty, locked, unregistered,
/// mid-operation (rebase/merge), or HEAD/tip-mismatched trees. Deletes the
/// branch atomically via `git update-ref -d` before deleting snapshot refs.
pub fn cleanup_verified_merged_task(
    repo_root: &Path,
    worktrees_dir: &Path,
    task_id: &str,
    expected_head: &str,
) -> Result<(), GitError> {
    validate_task_id(task_id)?;
    let expected_head = expected_head.trim();
    if !is_valid_oid(expected_head) {
        return Err(GitError::Invalid(format!(
            "invalid expected head oid {expected_head:?}"
        )));
    }
    let path = worktree_path(repo_root, worktrees_dir, task_id)?;
    let branch = format!("{BRANCH_PREFIX}{task_id}");
    if path.exists() || fs::symlink_metadata(&path).is_ok() {
        let wanted = fs::canonicalize(&path)?;
        let reg_entry = list(repo_root)?.into_iter().find(|entry| {
            entry.branch.as_deref() == Some(branch.as_str())
                && fs::canonicalize(&entry.path).is_ok_and(|p| p == wanted)
        });
        let Some(entry) = reg_entry else {
            return Err(GitError::Invalid(format!(
                "{} is not a worktree of branch {branch}",
                path.display()
            )));
        };
        if entry.locked {
            return Err(GitError::Invalid(format!(
                "worktree {} is locked",
                path.display()
            )));
        }
        if is_operation_in_progress(&path)? {
            return Err(GitError::Invalid(format!(
                "git operation in progress in {}",
                path.display()
            )));
        }
        ensure_clean(&path)?;
        let actual_head = git(&path, &["rev-parse", "HEAD"])?;
        if !actual_head.eq_ignore_ascii_case(expected_head) {
            return Err(GitError::Unmerged(format!(
                "{branch} (HEAD {actual_head} != expected {expected_head})"
            )));
        }
        let branch_ref = format!("refs/heads/{branch}");
        let branch_tip = git(repo_root, &["rev-parse", "--verify", &branch_ref])?;
        if !branch_tip.eq_ignore_ascii_case(expected_head) {
            return Err(GitError::Unmerged(format!(
                "{branch} (tip {branch_tip} != expected {expected_head})"
            )));
        }
        let submodules = removal_refusal(&path).is_ok_and(|r| r == Some(SUBMODULES_REFUSAL));
        if submodules {
            let args = ["status", "--porcelain", "--ignore-submodules=none"];
            if !git(&path, &args)?.is_empty() {
                return Err(GitError::Dirty(path.clone()));
            }
        }
        let path_s = path_str(&path)?;
        if submodules {
            git(repo_root, &["worktree", "remove", "--force", path_s])?;
        } else {
            git(repo_root, &["worktree", "remove", path_s])?;
        }
        delete_branch_if_at_head(repo_root, &branch, expected_head)?;
        delete_snapshot_refs(repo_root, task_id)?;
        return Ok(());
    }
    ensure_no_conflicting_worktree(repo_root, &path, &branch)?;
    prune(repo_root)?;
    delete_branch_if_at_head(repo_root, &branch, expected_head)?;
    delete_snapshot_refs(repo_root, task_id)?;
    Ok(())
}
