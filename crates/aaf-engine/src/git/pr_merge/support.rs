//! Support primitives for locked PR merge verification: scratch worktrees,
//! per-repo merge locking, trusted base config loading, CI polling, and fetch.

use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use aaf_domain::config::RepoConfig;

use super::super::cmd::{git, git_raw};
use super::super::gh_client::{GhClient, PrInfo};
use super::super::{GitError, check_branch_name, path_str};
use crate::throttle::Progress;

/// How often a waiter on the per-repo merge lock prints progress to stderr.
const MERGE_LOCK_PROGRESS_SECS: u64 = 15;
static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

pub(super) fn valid_sha40(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

pub(super) struct ScratchWorktree {
    repo_root: PathBuf,
    pub(super) path: PathBuf,
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
        git(
            repo_root,
            &["worktree", "add", "--detach", path_s, base_sha],
        )?;
        Ok(ScratchWorktree {
            repo_root: repo_root.to_path_buf(),
            path,
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
    }
}

pub(super) struct MergeLockGuard(File);

impl MergeLockGuard {
    /// Returns `Ok(Some((guard, waited)))` when acquired (`waited == true` if
    /// another merge held the lock at least once), or `Ok(None)` on timeout.
    pub(super) fn acquire(
        locks_dir: &Path,
        common_id: &str,
        pr_number: u64,
        timeout: Duration,
        progress: &dyn Fn(&Progress<'_>),
    ) -> Result<Option<(Self, bool)>, GitError> {
        crate::db::private::create_dir(locks_dir)?;
        let path = locks_dir.join(format!("{common_id}.merge.lock"));
        let file = crate::db::private::open_file(&path)?;
        let started = Instant::now();
        let deadline = started + timeout;
        let mut last_report: Option<Instant> = None;
        let mut waited = false;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Some((MergeLockGuard(file), waited))),
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    waited = true;
                    let now = Instant::now();
                    let should_report = last_report.is_none_or(|prev| {
                        now.saturating_duration_since(prev).as_secs() >= MERGE_LOCK_PROGRESS_SECS
                    });
                    if should_report {
                        last_report = Some(now);
                        progress(&Progress::WaitingForMergeLock {
                            pr_number,
                            waited_secs: now.saturating_duration_since(started).as_secs(),
                            timeout_secs: timeout.as_secs(),
                        });
                    }
                    let rem = deadline.saturating_duration_since(now);
                    thread::sleep(rem.min(Duration::from_millis(50)));
                }
                Err(TryLockError::WouldBlock) => return Ok(None),
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

pub(super) fn load_trusted_base_config(
    repo_root: &Path,
    remote: &str,
    base_branch: &str,
) -> Result<RepoConfig, GitError> {
    let spec = format!("{remote}/{base_branch}:.autofactory/config.toml");
    let raw = git(repo_root, &["show", &spec]).map_err(|_| {
        GitError::Gate(format!(
            "missing or unreadable .autofactory/config.toml on {remote}/{base_branch}"
        ))
    })?;
    let mut cfg: RepoConfig = toml::from_str(&raw).map_err(|e| {
        GitError::Gate(format!(
            "invalid .autofactory/config.toml on {remote}/{base_branch}: {e}"
        ))
    })?;
    cfg.cross_audit.require_different_provider = true;
    cfg.gate.base_ref = format!("{remote}/{base_branch}");
    Ok(cfg)
}

pub(super) fn check_ci(
    gh: &dyn GhClient,
    repo_root: &Path,
    pr_number: u64,
    wait_secs: u64,
) -> Result<Option<(String, String)>, GitError> {
    let deadline = Instant::now() + Duration::from_secs(wait_secs);
    loop {
        let checks = gh.pr_checks(repo_root, pr_number)?;
        let failed: Vec<&str> = checks
            .iter()
            .filter(|c| matches!(c.bucket.as_str(), "fail" | "cancel"))
            .map(|c| c.name.as_str())
            .collect();
        if !failed.is_empty() {
            return Ok(Some((
                "ci_failed".to_string(),
                format!("PR checks failed: {}", failed.join(", ")),
            )));
        }
        let pending: Vec<&str> = checks
            .iter()
            .filter(|c| c.bucket == "pending")
            .map(|c| c.name.as_str())
            .collect();
        if pending.is_empty() {
            return Ok(None);
        }
        if Instant::now() >= deadline {
            return Ok(Some((
                "ci_pending".to_string(),
                format!("PR checks still pending: {}", pending.join(", ")),
            )));
        }
        let rem = deadline.saturating_duration_since(Instant::now());
        thread::sleep(rem.min(Duration::from_secs(3)));
    }
}

pub(super) fn fetch_base_and_pr(
    repo_root: &Path,
    remote: &str,
    pr: &PrInfo,
    expect_head: &str,
) -> Result<(String, String), GitError> {
    check_branch_name(repo_root, &pr.base_ref_name)?;
    let base_refspec = format!(
        "+refs/heads/{0}:refs/remotes/{remote}/{0}",
        pr.base_ref_name
    );
    git(repo_root, &["fetch", "--quiet", remote, &base_refspec])?;
    let base_ref = format!("refs/remotes/{remote}/{}", pr.base_ref_name);
    let base_sha = git(repo_root, &["rev-parse", &base_ref])?;

    let tmp_ref = format!("refs/aaf-merge/pr-{}", pr.number);
    let pr_refspec = format!("+refs/pull/{}/head:{tmp_ref}", pr.number);
    let mut fetched_head =
        if git_raw(repo_root, &["fetch", "--quiet", remote, &pr_refspec])?.success() {
            git(repo_root, &["rev-parse", &tmp_ref]).ok()
        } else {
            None
        };
    // GitHub can update `refs/pull/<n>/head` asynchronously right after a push;
    // fall back to the branch ref (updated atomically by `git push`) if the PR
    // ref failed or still points at an older commit.
    if !fetched_head
        .as_deref()
        .is_some_and(|sha| sha.eq_ignore_ascii_case(expect_head))
    {
        check_branch_name(repo_root, &pr.head_ref_name)?;
        let branch_refspec = format!("+refs/heads/{}:{tmp_ref}", pr.head_ref_name);
        git(repo_root, &["fetch", "--quiet", remote, &branch_refspec])?;
        fetched_head = Some(git(repo_root, &["rev-parse", &tmp_ref])?);
    }
    let fetched_head = fetched_head
        .ok_or_else(|| GitError::Invalid(format!("failed to fetch PR #{} head", pr.number)))?;
    let _ = git_raw(repo_root, &["update-ref", "-d", &tmp_ref]);
    if !fetched_head.eq_ignore_ascii_case(expect_head) {
        return Err(GitError::Invalid(format!(
            "fetched PR head {fetched_head} does not match --expect-head {expect_head}"
        )));
    }
    Ok((base_sha, fetched_head))
}
