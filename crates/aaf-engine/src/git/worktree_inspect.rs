//! Read-only inspection and pruning helpers for git worktrees.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use super::GitError;
use super::cmd::{git, git_raw};
use super::worktree::{SUBMODULES_REFUSAL, WorktreeEntry};

/// Why a plain (non-`--force`) `git worktree remove` of `wt` would be
/// refused, mirroring git's own checks: a lock file, or initialised
/// submodules (`<git-dir>/modules`, or a gitlink checked out with a `.git`).
/// Lets a dry run report what the real run would keep.
pub fn removal_refusal(wt: &Path) -> Result<Option<&'static str>, GitError> {
    let git_dir = PathBuf::from(git(wt, &["rev-parse", "--absolute-git-dir"])?.trim());
    if git_dir.join("locked").exists() {
        return Ok(Some("cannot remove a locked working tree"));
    }
    if git_dir.join("modules").is_dir() {
        return Ok(Some(SUBMODULES_REFUSAL));
    }
    let index = git(wt, &["ls-files", "--stage", "-z"])?;
    let populated = index.split('\0').any(|entry| {
        entry.starts_with("160000 ")
            && entry
                .split_once('\t')
                .is_some_and(|(_, path)| wt.join(path).join(".git").exists())
    });
    Ok(populated.then_some(SUBMODULES_REFUSAL))
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
                locked: false,
            });
        } else if let Some(entry) = entries.last_mut() {
            if let Some(head) = field.strip_prefix("HEAD ") {
                entry.head = Some(head.to_string());
            } else if let Some(name) = field.strip_prefix("branch refs/heads/") {
                entry.branch = Some(name.to_string());
            } else if field == "locked" || field.starts_with("locked ") {
                entry.locked = true;
            }
        }
    }
    Ok(entries)
}

/// Drops the records of worktrees whose directory is gone.
pub fn prune(repo_root: &Path) -> Result<(), GitError> {
    git(repo_root, &["worktree", "prune"]).map(|_| ())
}

/// Drops the records of worktrees whose directory has been missing longer than
/// `expire` (e.g. `"5.minutes.ago"`), avoiding races with concurrent
/// `git worktree add` calls.
pub fn prune_with_expire(repo_root: &Path, expire: &str) -> Result<(), GitError> {
    let flag = format!("--expire={expire}");
    git(repo_root, &["worktree", "prune", &flag]).map(|_| ())
}

const IN_PROGRESS_MARKERS: [&str; 5] = [
    "rebase-merge",
    "rebase-apply",
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
];

/// Short branch names (without `refs/heads/`) currently undergoing an active
/// rebase, merge, cherry-pick, or revert in any registered worktree of
/// `repo_root`.
///
/// Covers detached-HEAD rebases by reading `<gitdir>/rebase-merge/head-name`
/// and `<gitdir>/rebase-apply/head-name`. Fails closed if an existing
/// worktree's gitdir cannot be inspected while a branch is checked out there.
pub fn active_in_progress_branches(repo_root: &Path) -> Result<HashSet<String>, GitError> {
    let entries = list(repo_root)?;
    Ok(active_in_progress_branches_for(&entries))
}

/// Like [`active_in_progress_branches`], reusing an already-fetched worktree list.
pub fn active_in_progress_branches_for(entries: &[WorktreeEntry]) -> HashSet<String> {
    let mut active = HashSet::new();
    for entry in entries {
        if !entry.path.exists() {
            continue;
        }
        let dot_git = entry.path.join(".git");
        let git_dir = if dot_git.is_dir() {
            dot_git
        } else {
            let Ok(git_dir_raw) = git(&entry.path, &["rev-parse", "--absolute-git-dir"]) else {
                if let Some(branch) = &entry.branch {
                    active.insert(branch.clone());
                }
                continue;
            };
            PathBuf::from(git_dir_raw.trim())
        };
        for rebase_dir in ["rebase-merge", "rebase-apply"] {
            let head_name = git_dir.join(rebase_dir).join("head-name");
            if let Ok(raw) = fs::read_to_string(&head_name) {
                let trimmed = raw.trim();
                let short = trimmed.strip_prefix("refs/heads/").unwrap_or(trimmed);
                if !short.is_empty() {
                    active.insert(short.to_string());
                }
            }
        }
        let has_marker = IN_PROGRESS_MARKERS
            .iter()
            .any(|marker| git_dir.join(marker).exists());
        if has_marker && let Some(branch) = &entry.branch {
            active.insert(branch.clone());
        }
    }
    active
}
