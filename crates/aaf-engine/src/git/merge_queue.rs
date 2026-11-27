//! Per-repository merge queue: one merge at a time, rebase onto the base,
//! re-run the gate on the rebased tree, then fast-forward. Never forces.

use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use super::cmd::{failure, git, git_raw};
use super::worktree::{self, TaskWorktree};
use super::{GitError, check_branch_name, has_remote};

#[derive(Debug, Clone)]
pub struct MergeRequest {
    /// Main work tree of the repository.
    pub repo_root: PathBuf,
    pub worktree: TaskWorktree,
    /// Short name of the branch to merge into, e.g. `main`.
    pub base: String,
    /// Remote to merge through when set and configured, e.g. `Some("origin")`.
    /// `None` forces a strictly local merge and never touches any remote.
    pub remote: Option<String>,
    /// Directory of the lock files (`<data dir>/locks`).
    pub locks_dir: PathBuf,
    /// How long to wait for another merge of the same repository.
    pub lock_timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    Merged {
        new_base_sha: String,
    },
    /// The rebase stopped on these files and was aborted.
    Conflict {
        files: Vec<String>,
    },
    GateFailed,
    /// The base moved after the rebase; nothing was forced. Retry.
    NotFastForward,
    /// Remote rejected the direct push (branch protection, ruleset, or server hook).
    PushRejected {
        reason: String,
    },
    /// Local-only merge could not fast-forward because the checked-out base
    /// working tree has uncommitted changes.
    BaseCheckoutDirty {
        path: PathBuf,
    },
}

/// `<locks_dir>/<repo_id>.merge.lock`.
pub fn lock_path(locks_dir: &Path, repo_root: &Path) -> PathBuf {
    locks_dir.join(format!("{}.merge.lock", aaf_fp::repo_id(repo_root)))
}

/// Holds the exclusive lock until dropped, so it is released on every
/// return path, panics included.
pub struct MergeLock {
    file: File,
    path: PathBuf,
}

impl MergeLock {
    fn acquire(path: &Path, timeout: Duration) -> Result<MergeLock, GitError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        let deadline = Instant::now() + timeout;
        loop {
            match file.try_lock() {
                Ok(()) => {
                    return Ok(MergeLock {
                        file,
                        path: path.to_path_buf(),
                    });
                }
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(TryLockError::WouldBlock) => {
                    return Err(GitError::LockTimeout(path.to_path_buf()));
                }
                Err(TryLockError::Error(err)) => return Err(err.into()),
            }
        }
    }
}

impl Drop for MergeLock {
    fn drop(&mut self) {
        // Closing the file releases the lock anyway; this only makes the
        // release immediate.
        let _ = self.file.unlock();
    }
}

/// Acquires the cross-process merge lock for `req.repo_root` after validating
/// `req.worktree` and `req.base`. Callers that also hold an in-process mutex
/// can acquire this lock first so waiting on another process never stalls
/// sibling worker threads.
pub fn acquire_lock(req: &MergeRequest) -> Result<MergeLock, GitError> {
    req.worktree.task_id()?;
    check_branch_name(&req.repo_root, &req.base)?;
    let lock = lock_path(&req.locks_dir, &req.repo_root);
    MergeLock::acquire(&lock, req.lock_timeout)
}

/// Runs the merge queue with an already-acquired [`MergeLock`], passing the
/// exact resolved `onto_sha` onto which the task branch was rebased to `regate`.
pub fn merge_with_lock_onto(
    req: &MergeRequest,
    lock: &MergeLock,
    regate: &dyn Fn(&Path, &str) -> Result<bool, String>,
) -> Result<MergeOutcome, GitError> {
    let expected = lock_path(&req.locks_dir, &req.repo_root);
    if lock.path != expected {
        return Err(GitError::Invalid(format!(
            "merge lock {} does not match {}",
            lock.path.display(),
            expected.display()
        )));
    }
    req.worktree.task_id()?;
    check_branch_name(&req.repo_root, &req.base)?;
    merge_locked(req, regate)
}

/// Merges the task branch into the base. `regate` runs on the rebased
/// worktree and decides whether the result may land.
pub fn merge(
    req: &MergeRequest,
    regate: &dyn Fn(&Path) -> Result<bool, String>,
) -> Result<MergeOutcome, GitError> {
    let _lock = acquire_lock(req)?;
    merge_locked(req, &|tree, _| regate(tree))
}

fn merge_locked(
    req: &MergeRequest,
    regate: &dyn Fn(&Path, &str) -> Result<bool, String>,
) -> Result<MergeOutcome, GitError> {
    let tree = req.worktree.path.as_path();
    let branch = req.worktree.branch.as_str();
    if git(tree, &["symbolic-ref", "--short", "HEAD"])? != branch {
        return Err(GitError::Invalid(format!(
            "{} is not on branch {branch}",
            tree.display()
        )));
    }
    // Uncommitted work would be neither rebased nor gated nor merged.
    worktree::ensure_clean(tree)?;

    let remote_name = match req.remote.as_deref() {
        Some(name) if has_remote(&req.repo_root, name)? => Some(name),
        _ => None,
    };
    let onto = if let Some(remote) = remote_name {
        let tracking = format!("refs/remotes/{remote}/{}", req.base);
        let refspec = format!("+refs/heads/{}:{tracking}", req.base);
        git(&req.repo_root, &["fetch", remote, &refspec])?;
        tracking
    } else {
        format!("refs/heads/{}", req.base)
    };

    let onto_sha = git(&req.repo_root, &["rev-parse", "--verify", &onto])?;
    super::pseudo_ref_guard::ensure_plain_pseudo_refs(tree)?;
    // Onto the resolved commit, not the ref: a ref that moved in between
    // would land the task on a base the gate is not told about.
    if let Some(files) = rebase(tree, &onto_sha)? {
        return Ok(MergeOutcome::Conflict { files });
    }
    // Hooks are off for the rebase, but anything that still left the tree
    // differing from HEAD would let the gate judge one tree and land another.
    worktree::ensure_clean(tree)?;
    // The exact commit the gate is about to judge is the one that lands.
    let head = git(tree, &["rev-parse", "HEAD"])?;
    match regate(tree, &onto_sha) {
        Ok(true) => {}
        Ok(false) => return Ok(MergeOutcome::GateFailed),
        Err(reason) => return Err(GitError::Gate(reason)),
    }

    if let Some(remote) = remote_name {
        push_fast_forward(req, remote, &head)
    } else {
        local_fast_forward(req, &head)
    }
}

/// Rebases the checked-out branch onto `onto`. Returns the conflicting
/// files after aborting when the rebase stopped on a conflict. Hooks are off:
/// the task's own (in-tree `core.hooksPath`) would run with the gate's trust
/// and could rewrite the working tree the gate is about to judge.
fn rebase(tree: &Path, onto: &str) -> Result<Option<Vec<String>>, GitError> {
    let args = [
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "commit.gpgsign=false",
        "rebase",
        "--rebase-merges",
        onto,
    ];
    let out = git_raw(tree, &args)?;
    if out.success() {
        return Ok(None);
    }
    if !rebase_in_progress(tree)? {
        return Err(failure(&args, &out));
    }
    let unmerged = git_raw(tree, &["diff", "--name-only", "--diff-filter=U", "-z"])?;
    let files: Vec<String> = unmerged
        .stdout
        .split('\0')
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect();
    git(
        tree,
        &["-c", "core.hooksPath=/dev/null", "rebase", "--abort"],
    )?;
    if files.is_empty() {
        // Stopped for a reason other than a content conflict.
        return Err(failure(&args, &out));
    }
    Ok(Some(files))
}

fn rebase_in_progress(tree: &Path) -> Result<bool, GitError> {
    for state in ["rebase-merge", "rebase-apply"] {
        let dir = git(tree, &["rev-parse", "--git-path", state])?;
        // Git prints the path relative to the directory it ran in.
        if tree.join(dir).exists() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn is_push_protected_rejection(stdout: &str, stderr: &str) -> bool {
    let lower_out = stdout.to_ascii_lowercase();
    let lower_err = stderr.to_ascii_lowercase();
    stdout
        .lines()
        .any(|line| line.starts_with("!\t") && line.contains("[remote rejected]"))
        || lower_err.contains("gh006")
        || lower_err.contains("gh013")
        || lower_err.contains("protected branch hook declined")
        || lower_err.contains("protected branch update failed")
        || lower_err.contains("repository rule violations")
        || lower_err.contains("remote rejected")
        || lower_out.contains("protected branch hook declined")
}

fn push_fast_forward(
    req: &MergeRequest,
    remote: &str,
    head: &str,
) -> Result<MergeOutcome, GitError> {
    let refspec = format!("{head}:refs/heads/{}", req.base);
    let args = ["push", "--porcelain", remote, &refspec];
    let out = git_raw(&req.repo_root, &args)?;
    if out.success() {
        return Ok(MergeOutcome::Merged {
            new_base_sha: head.to_string(),
        });
    }
    if is_push_protected_rejection(&out.stdout, &out.stderr) {
        let detail = out.stderr.trim();
        let reason = if detail.is_empty() {
            out.stdout.trim().to_string()
        } else {
            detail.to_string()
        };
        return Ok(MergeOutcome::PushRejected { reason });
    }
    // `!<TAB>src:dst<TAB>[rejected] (...)` is the remote refusing a
    // non-fast-forward; any other failure (network, hook) is an error.
    let rejected = out
        .stdout
        .lines()
        .any(|line| line.starts_with("!\t") && line.contains("[rejected]"));
    if rejected {
        Ok(MergeOutcome::NotFastForward)
    } else {
        Err(failure(&args, &out))
    }
}

fn local_fast_forward(req: &MergeRequest, head: &str) -> Result<MergeOutcome, GitError> {
    let repo = req.repo_root.as_path();
    let base_ref = format!("refs/heads/{}", req.base);
    let old = git(repo, &["rev-parse", "--verify", &base_ref])?;
    let args = ["merge-base", "--is-ancestor", &old, head];
    let ancestry = git_raw(repo, &args)?;
    match ancestry.code {
        Some(0) => {}
        Some(1) => return Ok(MergeOutcome::NotFastForward),
        _ => return Err(failure(&args, &ancestry)),
    }
    let checked_out = worktree::list(repo)?
        .into_iter()
        .find(|entry| entry.branch.as_deref() == Some(req.base.as_str()));
    match checked_out {
        Some(entry) => {
            // Moving the ref under a checked-out tree would make its files
            // look like a revert, so the tree itself is fast-forwarded, and
            // only when no tracked change could get mixed in.
            let status = git(&entry.path, &["status", "--porcelain", "-uno"])?;
            if !status.is_empty() {
                return Ok(MergeOutcome::BaseCheckoutDirty { path: entry.path });
            }
            git(&entry.path, &["merge", "--ff-only", head])?;
        }
        None => {
            // Compare-and-swap on the old value: a base that moved since the
            // ancestry check is left alone.
            if !git_raw(repo, &["update-ref", &base_ref, head, &old])?.success() {
                return Ok(MergeOutcome::NotFastForward);
            }
        }
    }
    Ok(MergeOutcome::Merged {
        new_base_sha: head.to_string(),
    })
}
