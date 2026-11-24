//! RAII guards and bounded deadline helpers for [`super::pr_merge`].

use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use aaf_domain::config::RepoConfig;

use super::cmd::{git, git_raw};
use super::gh_client::PrInfo;
use super::{GitError, check_branch_name, path_str};
use crate::config::{self, REPO_CONFIG_REL};

/// Maximum wait duration (24 hours) for lock or CI polling to prevent overflow
/// when computing `Instant` deadlines.
pub(super) const MAX_WAIT_SECS: u64 = 86_400;

static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

/// Returns a bounded deadline that never overflows `Instant`.
pub(super) fn bounded_deadline(timeout: Duration) -> Instant {
    let clamped = timeout.min(Duration::from_secs(MAX_WAIT_SECS));
    let now = Instant::now();
    now.checked_add(clamped).unwrap_or(now)
}

/// Tier 2 from the immutable `<base_sha>` (`git show`: never the PR's or the
/// checkout's copy), with the tighten-only tier-1, tier-3 (from `repo_root`)
/// and `AAF_*` environment overlays layered on top exactly as for `aaf gate`.
pub(super) fn load_trusted_base_config(
    repo_root: &Path,
    base_sha: &str,
) -> Result<RepoConfig, GitError> {
    let spec = format!("{base_sha}:{REPO_CONFIG_REL}");
    let raw = git(repo_root, &["show", &spec]).map_err(|_| {
        GitError::Gate(format!(
            "missing or unreadable {REPO_CONFIG_REL} at base {base_sha}"
        ))
    })?;
    let base: RepoConfig = toml::from_str(&raw).map_err(|e| {
        GitError::Gate(format!("invalid {REPO_CONFIG_REL} at base {base_sha}: {e}"))
    })?;
    config::load_over_repo_from_env(repo_root, base, &spec)
        .map(|effective| effective.repo)
        .map_err(|e| GitError::Gate(format!("config over base {base_sha}: {e}")))
}

/// Isolated detached scratch worktree for verifying a merged PR tree; always
/// removed and pruned on `Drop`. Holds an exclusive advisory lock on
/// `<path>.lock` for its entire lifetime so `aaf gc` never deletes a live
/// scratch worktree during optimistic pre-merge verification.
pub(super) struct ScratchWorktree {
    repo_root: PathBuf,
    pub(super) path: PathBuf,
    lock_path: PathBuf,
    lock_file: Option<File>,
}

impl ScratchWorktree {
    pub(super) fn create(
        repo_root: &Path,
        data_dir: &Path,
        common_id: &str,
        pr_number: u64,
        base_sha: &str,
    ) -> Result<ScratchWorktree, GitError> {
        let base_dir = data_dir.join("merge-scratch");
        fs::create_dir_all(&base_dir)?;
        let seq = SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = base_dir.join(format!(
            "{common_id}-pr{pr_number}-{}-{seq}",
            std::process::id()
        ));
        let path_s = path_str(&path)?;
        let mut lock_os = path.as_os_str().to_os_string();
        lock_os.push(".lock");
        let lock_path = PathBuf::from(lock_os);
        let lock_file = crate::db::private::open_file(&lock_path)?;
        match lock_file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(GitError::LockTimeout(lock_path)),
            Err(TryLockError::Error(err)) => return Err(err.into()),
        }
        if let Err(err) = git(
            repo_root,
            &[
                "-c",
                "core.hooksPath=/dev/null",
                "worktree",
                "add",
                "--detach",
                path_s,
                base_sha,
            ],
        ) {
            let _ = lock_file.unlock();
            drop(lock_file);
            let _ = fs::remove_file(&lock_path);
            return Err(err);
        }
        Ok(ScratchWorktree {
            repo_root: repo_root.to_path_buf(),
            path,
            lock_path,
            lock_file: Some(lock_file),
        })
    }
}

impl Drop for ScratchWorktree {
    fn drop(&mut self) {
        if let Some(p) = self.path.to_str() {
            let _ = git_raw(&self.repo_root, &["worktree", "remove", "--force", p]);
        }
        let _ = fs::remove_dir_all(&self.path);
        let _ = git_raw(&self.repo_root, &["worktree", "prune"]);
        if let Some(file) = self.lock_file.take() {
            let _ = file.unlock();
            drop(file);
        }
        let _ = fs::remove_file(&self.lock_path);
    }
}

/// Per-repository exclusive merge lock; always unlocked on `Drop`.
pub(super) struct MergeLockGuard(File);

impl MergeLockGuard {
    pub(super) fn acquire(
        locks_dir: &Path,
        common_id: &str,
        timeout: Duration,
    ) -> Result<Self, GitError> {
        fs::create_dir_all(locks_dir)?;
        let path = locks_dir.join(format!("{common_id}.merge.lock"));
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)?;
        let deadline = bounded_deadline(timeout);
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(MergeLockGuard(file)),
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(50));
                }
                Err(TryLockError::WouldBlock) => return Err(GitError::LockTimeout(path)),
                Err(TryLockError::Error(err)) => return Err(err.into()),
            }
        }
    }
}

impl Drop for MergeLockGuard {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Unique temporary ref (`refs/aaf-merge/pr-{num}-{pid}-{seq}`); always
/// deleted on `Drop` so error paths never leak refs.
pub(super) struct TempRefGuard {
    repo_root: PathBuf,
    ref_name: String,
}

impl TempRefGuard {
    pub(super) fn new(repo_root: &Path, pr_number: u64) -> Self {
        let seq = SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed);
        let ref_name = format!("refs/aaf-merge/pr-{pr_number}-{}-{seq}", std::process::id());
        Self {
            repo_root: repo_root.to_path_buf(),
            ref_name,
        }
    }

    pub(super) fn as_str(&self) -> &str {
        &self.ref_name
    }
}

impl Drop for TempRefGuard {
    fn drop(&mut self) {
        let _ = git_raw(&self.repo_root, &["update-ref", "-d", &self.ref_name]);
    }
}

pub(super) fn fetch_base(
    repo_root: &Path,
    remote: &str,
    base_ref_name: &str,
) -> Result<String, GitError> {
    check_branch_name(repo_root, base_ref_name)?;
    let base_refspec = format!("+refs/heads/{base_ref_name}:refs/remotes/{remote}/{base_ref_name}");
    git(
        repo_root,
        &["fetch", "--quiet", "--", remote, &base_refspec],
    )?;
    let base_ref = format!("refs/remotes/{remote}/{base_ref_name}");
    git(repo_root, &["rev-parse", &base_ref])
}

pub(super) fn has_local_commit(repo_root: &Path, sha: &str) -> Result<bool, GitError> {
    let spec = format!("{sha}^{{commit}}");
    Ok(git_raw(repo_root, &["cat-file", "-e", &spec])?.success())
}

pub(super) fn has_local_commit_and_tree(repo_root: &Path, sha: &str) -> Result<bool, GitError> {
    let tree_spec = format!("{sha}^{{tree}}");
    Ok(has_local_commit(repo_root, sha)?
        && git_raw(repo_root, &["cat-file", "-e", &tree_spec])?.success())
}

pub(super) fn fetch_base_and_pr(
    repo_root: &Path,
    remote: &str,
    pr: &PrInfo,
    expect_head: &str,
) -> Result<(String, String), GitError> {
    if pr.head_ref_oid.eq_ignore_ascii_case(expect_head)
        && has_local_commit_and_tree(repo_root, expect_head)?
    {
        let base_sha = fetch_base(repo_root, remote, &pr.base_ref_name)?;
        let commit_spec = format!("{expect_head}^{{commit}}");
        let head_sha = git(repo_root, &["rev-parse", "--verify", &commit_spec])?;
        return Ok((base_sha, head_sha));
    }

    check_branch_name(repo_root, &pr.base_ref_name)?;
    let base_refspec = format!(
        "+refs/heads/{}:refs/remotes/{remote}/{}",
        pr.base_ref_name, pr.base_ref_name
    );
    let base_ref = format!("refs/remotes/{remote}/{}", pr.base_ref_name);
    let tmp_ref = TempRefGuard::new(repo_root, pr.number);
    let pr_refspec = format!("+refs/pull/{}/head:{}", pr.number, tmp_ref.as_str());

    // Combine base + PR refspecs into a single network fetch; fall back to
    // separate fetches when `refs/pull/<n>/head` is missing or lagging.
    let (base_sha, mut fetched_head) = if git_raw(
        repo_root,
        &["fetch", "--quiet", "--", remote, &base_refspec, &pr_refspec],
    )?
    .success()
    {
        let base_sha = git(repo_root, &["rev-parse", &base_ref])?;
        let head = git(repo_root, &["rev-parse", tmp_ref.as_str()]).ok();
        (base_sha, head)
    } else {
        let base_sha = fetch_base(repo_root, remote, &pr.base_ref_name)?;
        let head =
            if git_raw(repo_root, &["fetch", "--quiet", "--", remote, &pr_refspec])?.success() {
                git(repo_root, &["rev-parse", tmp_ref.as_str()]).ok()
            } else {
                None
            };
        (base_sha, head)
    };

    // GitHub can update `refs/pull/<n>/head` asynchronously right after a push;
    // try the branch ref (updated atomically by `git push`) as a best-effort
    // fallback if the PR ref failed or still points at an older commit (for fork
    // PRs the branch does not exist on `remote`, so a miss is non-fatal).
    if !fetched_head
        .as_deref()
        .is_some_and(|sha| sha.eq_ignore_ascii_case(expect_head))
        && check_branch_name(repo_root, &pr.head_ref_name).is_ok()
    {
        let branch_refspec = format!("+refs/heads/{}:{}", pr.head_ref_name, tmp_ref.as_str());
        if git_raw(
            repo_root,
            &["fetch", "--quiet", "--", remote, &branch_refspec],
        )?
        .success()
            && let Ok(sha) = git(repo_root, &["rev-parse", tmp_ref.as_str()])
        {
            fetched_head = Some(sha);
        }
    }
    let fetched_head = fetched_head
        .ok_or_else(|| GitError::Invalid(format!("failed to fetch PR #{} head", pr.number)))?;
    if !fetched_head.eq_ignore_ascii_case(expect_head) {
        return Err(GitError::Invalid(format!(
            "fetched PR head {fetched_head} does not match --expect-head {expect_head}"
        )));
    }
    Ok((base_sha, fetched_head))
}
