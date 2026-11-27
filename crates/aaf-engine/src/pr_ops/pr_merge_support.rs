//! RAII guards, in-memory merge pre-check, lock/CI helpers, and bounded
//! deadline utilities for [`super::pr_merge`].

use std::fs::{self, File, TryLockError};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use aaf_domain::config::{GlobalConfig, RepoConfig};

use crate::config::{self, REPO_CONFIG_REL};
use crate::git::cmd::{failure, git, git_raw};
use crate::git::gh_client::{GhClient, PrInfo};
use crate::git::hydration::load_base_worktree_config;
use crate::git::warm_slot::{
    SlotKind, WarmSlot, base_snapshots_dir, promote_from_pure_base_slot,
    restore_clean_base_snapshot,
};
use crate::git::{GitError, check_branch_name, worktree};
use crate::throttle::Progress;

pub(super) use super::pr_temp_ref::TempRefGuard;

/// Maximum wait duration (24 hours) for lock or CI polling to prevent overflow
/// when computing `Instant` deadlines.
pub(super) const MAX_WAIT_SECS: u64 = 86_400;
/// How often a waiter on the per-repo merge lock reports progress.
const MERGE_LOCK_PROGRESS_SECS: u64 = 15;

/// Returns a bounded deadline that never overflows `Instant`.
pub(super) fn bounded_deadline(timeout: Duration) -> Instant {
    let clamped = timeout.min(Duration::from_secs(MAX_WAIT_SECS));
    let now = Instant::now();
    now.checked_add(clamped).unwrap_or(now)
}

pub(super) fn valid_sha40(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Isolated detached worktree for verifying a merged PR tree, backed by a
/// fixed-path persistent [`WarmSlot`] (with ephemeral fallback under contention).
pub(super) struct ScratchWorktree {
    slot: WarmSlot,
}

impl ScratchWorktree {
    pub(super) fn create(
        repo_root: &Path,
        data_dir: &Path,
        common_id: &str,
        pr_number: u64,
        base_sha: &str,
    ) -> Result<ScratchWorktree, GitError> {
        worktree::ensure_plain_reflogs(repo_root)?;
        let slot = WarmSlot::acquire(
            repo_root,
            data_dir,
            common_id,
            SlotKind::Merge,
            base_sha,
            &format!("pr{pr_number}"),
        )?;
        let scratch = ScratchWorktree { slot };
        if let Ok(wt_cfg) = load_base_worktree_config(repo_root, base_sha)
            && !wt_cfg.clone_dirs.is_empty()
        {
            let dirs: Vec<&str> = wt_cfg.clone_dirs.iter().map(String::as_str).collect();
            scratch.seed_from_base_snapshot(data_dir, common_id, base_sha, &dirs);
        }
        Ok(scratch)
    }

    pub(super) fn path(&self) -> &Path {
        self.slot.path()
    }

    /// Best-effort CoW restore of pre-warmed base artifact directories into
    /// this merge verification slot from the shared base snapshots directory.
    pub(super) fn seed_from_base_snapshot(
        &self,
        data_dir: &Path,
        common_id: &str,
        base_sha: &str,
        artifact_dirs: &[&str],
    ) -> usize {
        let snaps_dir = base_snapshots_dir(data_dir, common_id);
        restore_clean_base_snapshot(self.slot.path(), &snaps_dir, base_sha, artifact_dirs)
            .unwrap_or(0)
    }

    /// Promotes build artifact directories ONLY when the underlying slot is at
    /// pure `base_sha` with a clean working tree (never from a merged PR tree).
    #[allow(dead_code)]
    pub(super) fn promote_base_artifacts(
        &self,
        data_dir: &Path,
        common_id: &str,
        base_sha: &str,
        artifact_dirs: &[&str],
    ) -> Result<(), GitError> {
        promote_from_pure_base_slot(&self.slot, data_dir, common_id, base_sha, artifact_dirs)
    }
}

/// Outcome of an in-memory `git merge-tree --write-tree` pre-check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum InMemoryMerge {
    Clean { tree_oid: String },
    Conflict { detail: String },
    Unsupported,
}

/// Computes the merged git tree OID in memory without creating or touching any
/// working tree. Falls back to [`InMemoryMerge::Unsupported`] on older git
/// versions lacking `--write-tree`.
pub(super) fn in_memory_merge_tree(
    repo_root: &Path,
    base_sha: &str,
    head_sha: &str,
) -> Result<InMemoryMerge, GitError> {
    let args = [
        "merge-tree",
        "--write-tree",
        "--no-messages",
        base_sha,
        head_sha,
    ];
    let out = git_raw(repo_root, &args)?;
    match out.code {
        Some(0) => {
            let first = out
                .stdout
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .unwrap_or("");
            if valid_sha40(first) {
                Ok(InMemoryMerge::Clean {
                    tree_oid: first.to_string(),
                })
            } else {
                Ok(InMemoryMerge::Unsupported)
            }
        }
        Some(1) => {
            let mut files: Vec<&str> = Vec::new();
            for line in out.stdout.lines().skip(1) {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let name = trimmed.split_once('\t').map_or(trimmed, |(_, path)| path);
                if !files.contains(&name) {
                    files.push(name);
                }
            }
            let detail = if !files.is_empty() {
                files.join(", ")
            } else if !out.stderr.trim().is_empty() {
                out.stderr.trim().to_string()
            } else {
                "merge conflict".to_string()
            };
            Ok(InMemoryMerge::Conflict { detail })
        }
        Some(129) => Ok(InMemoryMerge::Unsupported),
        _ => {
            if out.stderr.contains("unknown option") || out.stderr.contains("usage:") {
                Ok(InMemoryMerge::Unsupported)
            } else if out.stderr.contains("unrelated histories") {
                Ok(InMemoryMerge::Conflict {
                    detail: out.stderr.trim().to_string(),
                })
            } else {
                Err(failure(&args, &out))
            }
        }
    }
}

/// Lightweight `git ls-remote --heads <remote> refs/heads/<branch>` tip check.
/// Returns `Ok(None)` when empty or non-zero so callers safely fall back to a
/// full `fetch_base_and_pr`.
pub(super) fn ls_remote_head(
    repo_root: &Path,
    remote: &str,
    branch: &str,
) -> Result<Option<String>, GitError> {
    check_branch_name(repo_root, branch)?;
    if remote.is_empty() || remote.starts_with('-') {
        return Err(GitError::Invalid(format!("remote name {remote:?}")));
    }
    let target_ref = format!("refs/heads/{branch}");
    let out = git_raw(repo_root, &["ls-remote", "--heads", remote, &target_ref])?;
    if !out.success() {
        return Ok(None);
    }
    for line in out.stdout.lines() {
        let mut parts = line.split_whitespace();
        let Some(sha) = parts.next() else { continue };
        let Some(refname) = parts.next() else {
            continue;
        };
        if refname == target_ref && valid_sha40(sha) {
            return Ok(Some(sha.to_string()));
        }
    }
    Ok(None)
}

/// Tier 2 from the immutable `<base_sha>` (`git show`: never the PR's or the
/// checkout's copy), with the tighten-only tier-1, tier-3 (from `repo_root`)
/// and `AAF_*` environment overlays layered on top exactly as for `aaf gate`
/// (never loosening). Tier 1 is `global` when the caller already loaded it,
/// else the global config file itself: a missing `global` never skips it.
pub(super) fn load_trusted_base_config(
    repo_root: &Path,
    base_sha: &str,
    global: Option<&GlobalConfig>,
) -> Result<RepoConfig, GitError> {
    let spec = format!("{base_sha}:{REPO_CONFIG_REL}");
    let raw = git(repo_root, &["show", &spec]).map_err(|_| {
        GitError::Gate(format!(
            "missing or unreadable {REPO_CONFIG_REL} at base {base_sha}"
        ))
    })?;
    let base: RepoConfig = aaf_domain::builtin_defaults::parse_repo(&raw).map_err(|e| {
        GitError::Gate(format!("invalid {REPO_CONFIG_REL} at base {base_sha}: {e}"))
    })?;
    let tightened = match global {
        Some(global) => config::tighten_trusted_base_from_env(base, global, Some(repo_root)),
        None => config::load_over_repo_from_env(repo_root, base, &spec).map(|eff| eff.repo),
    };
    tightened.map_err(|e| GitError::Gate(format!("config over base {base_sha}: {e}")))
}

pub(super) fn check_ci(
    gh: &dyn GhClient,
    repo_root: &Path,
    pr_number: u64,
    wait_secs: u64,
    require_checks: bool,
) -> Result<(Option<(String, String)>, bool), GitError> {
    let deadline = bounded_deadline(Duration::from_secs(wait_secs.min(MAX_WAIT_SECS)));
    let mut waited = false;
    loop {
        let checks = gh.pr_checks(repo_root, pr_number)?;
        let failed: Vec<&str> = checks
            .iter()
            .filter(|c| matches!(c.bucket.as_str(), "fail" | "cancel"))
            .map(|c| c.name.as_str())
            .collect();
        if !failed.is_empty() {
            return Ok((
                Some((
                    "ci_failed".to_string(),
                    format!("PR checks failed: {}", failed.join(", ")),
                )),
                waited,
            ));
        }
        let mut pending: Vec<&str> = checks
            .iter()
            .filter(|c| c.bucket == "pending")
            .map(|c| c.name.as_str())
            .collect();
        if require_checks && checks.is_empty() {
            pending.push("(no check registered yet)");
        }
        if pending.is_empty() {
            return Ok((None, waited));
        }
        if Instant::now() >= deadline {
            return Ok((
                Some((
                    "ci_pending".to_string(),
                    format!("PR checks still pending: {}", pending.join(", ")),
                )),
                waited,
            ));
        }
        waited = true;
        let rem = deadline.saturating_duration_since(Instant::now());
        thread::sleep(rem.min(Duration::from_secs(3)));
    }
}

/// Acquires the per-repo merge lock with private permissions, bounded deadline,
/// and periodic `WaitingForMergeLock` progress reports. Returns `Ok(None)` on timeout.
pub(super) fn acquire_merge_lock(
    locks_dir: &Path,
    common_id: &str,
    pr_number: u64,
    timeout: Duration,
    progress: &dyn Fn(&Progress<'_>),
) -> Result<Option<(MergeLockGuard, bool)>, GitError> {
    aaf_fp::fs_private::create_dir(locks_dir)?;
    let lock_path = locks_dir.join(format!("{common_id}.merge.lock"));
    drop(aaf_fp::fs_private::open_file(&lock_path)?);
    let started = Instant::now();
    let deadline = bounded_deadline(timeout);
    let mut last_report: Option<Instant> = None;
    let mut waited = false;
    loop {
        match MergeLockGuard::acquire(locks_dir, common_id, Duration::ZERO) {
            Ok(guard) => return Ok(Some((guard, waited))),
            Err(GitError::LockTimeout(_)) if Instant::now() < deadline => {
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
            Err(GitError::LockTimeout(_)) => return Ok(None),
            Err(err) => return Err(err),
        }
    }
}

pub(super) fn sync_checked_out_local_base(
    repo_root: &Path,
    remote: &str,
    base_branch: &str,
) -> String {
    let refspec = format!("+refs/heads/{base_branch}:refs/remotes/{remote}/{base_branch}");
    if git(repo_root, &["fetch", "--quiet", "--", remote, &refspec]).is_err() {
        return format!("warning: could not fetch {remote}/{base_branch} after merge");
    }
    let current = git(repo_root, &["symbolic-ref", "--short", "HEAD"]).unwrap_or_default();
    if current != base_branch {
        return format!(
            "local checkout is on `{current}` (not `{base_branch}`); skipped local fast-forward"
        );
    }
    let dirty = git(repo_root, &["status", "--porcelain"]).unwrap_or_else(|_| "dirty".into());
    if !dirty.is_empty() {
        return format!(
            "warning: local `{base_branch}` has uncommitted changes; skipped local fast-forward"
        );
    }
    let target = format!("{remote}/{base_branch}");
    // Full refname: a short `origin/main` resolves `refs/tags/origin/main`
    // first, so a planted tag could fast-forward the base (SEC-035).
    let target_ref = format!("refs/remotes/{remote}/{base_branch}");
    // SEC-125: `merge --ff-only` writes `ORIG_HEAD`; a symref there moves another ref.
    if let Err(e) = crate::git::pseudo_ref_guard::ensure_plain_pseudo_refs(repo_root) {
        return format!("warning: skipped local fast-forward of `{base_branch}`: {e}");
    }
    match git_raw(
        repo_root,
        &["merge", "--ff-only", "--no-overwrite-ignore", &target_ref],
    ) {
        Ok(st) if st.success() => format!("fast-forwarded local `{base_branch}` to `{target}`"),
        _ => format!(
            "warning: local `{base_branch}` could not fast-forward to `{target}` (diverged)"
        ),
    }
}

/// Fetches `remote/base_branch` and fast-forwards local `refs/heads/<base_branch>`:
/// - when `HEAD == base_branch`, delegates to clean checked-out `--ff-only` merge;
/// - when `HEAD != base_branch` and `worktree::list` confirms no worktree has
///   `base_branch` checked out, fast-forwards via `--is-ancestor` + CAS `update-ref`.
pub(super) fn sync_local_base(repo_root: &Path, remote: &str, base_branch: &str) -> String {
    if let Err(err) = worktree::ensure_plain_reflogs(repo_root) {
        return format!("warning: skipped local base sync: {err}");
    }
    let current = git(repo_root, &["symbolic-ref", "--short", "HEAD"]).unwrap_or_default();
    if current == base_branch {
        return sync_checked_out_local_base(repo_root, remote, base_branch);
    }
    let refspec = format!("+refs/heads/{base_branch}:refs/remotes/{remote}/{base_branch}");
    if git(repo_root, &["fetch", "--quiet", "--", remote, &refspec]).is_err() {
        return format!("warning: could not fetch {remote}/{base_branch} after merge");
    }
    let checked_out_elsewhere = worktree::list(repo_root)
        .map(|entries| {
            entries
                .into_iter()
                .any(|entry| entry.branch.as_deref() == Some(base_branch))
        })
        .unwrap_or(true);
    if checked_out_elsewhere {
        return format!(
            "local branch `{base_branch}` is checked out in another worktree; skipped local fast-forward"
        );
    }
    let remote_ref = format!("refs/remotes/{remote}/{base_branch}");
    let local_ref = format!("refs/heads/{base_branch}");
    let Ok(new_sha) = git(repo_root, &["rev-parse", "--verify", &remote_ref]) else {
        return format!("warning: could not resolve `{remote_ref}` after fetch");
    };
    let Ok(old_sha) = git(repo_root, &["rev-parse", "--verify", &local_ref]) else {
        return format!("local branch `{base_branch}` does not exist; skipped local fast-forward");
    };
    if old_sha == new_sha {
        return format!("local `{base_branch}` already up to date with `{remote}/{base_branch}`");
    }
    let is_ancestor = git_raw(
        repo_root,
        &["merge-base", "--is-ancestor", &old_sha, &new_sha],
    )
    .is_ok_and(|out| out.code == Some(0));
    if !is_ancestor {
        return format!(
            "warning: local `{base_branch}` could not fast-forward to `{remote}/{base_branch}` (diverged)"
        );
    }
    match git_raw(repo_root, &["update-ref", &local_ref, &new_sha, &old_sha]) {
        Ok(out) if out.success() => format!(
            "fast-forwarded local `{base_branch}` to `{remote}/{base_branch}` via update-ref"
        ),
        _ => format!(
            "warning: local `{base_branch}` moved concurrently; skipped update-ref to `{remote}/{base_branch}`"
        ),
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

pub(super) fn fetch_base(
    repo_root: &Path,
    remote: &str,
    base_ref_name: &str,
) -> Result<String, GitError> {
    check_branch_name(repo_root, base_ref_name)?;
    worktree::ensure_plain_reflogs(repo_root)?;
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
    worktree::ensure_plain_reflogs(repo_root)?;
    let base_refspec = format!(
        "+refs/heads/{}:refs/remotes/{remote}/{}",
        pr.base_ref_name, pr.base_ref_name
    );
    let base_ref = format!("refs/remotes/{remote}/{}", pr.base_ref_name);
    let tmp_ref = TempRefGuard::new(repo_root, pr.number);
    let pr_refspec = format!("+refs/pull/{}/head:{}", pr.number, tmp_ref.as_str());

    // Combine base + PR refspecs into a single network fetch; fall back to
    // separate fetches when `refs/pull/<n>/head` is missing or lagging.
    // SEC-135: reject a pre-planted symref/symlink/hard-link at `tmp_ref`
    // before fetching so git cannot follow it onto another branch.
    crate::git::pseudo_ref_guard::ensure_plain_branch_ref(repo_root, tmp_ref.as_str())?;
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
        crate::git::pseudo_ref_guard::ensure_plain_branch_ref(repo_root, tmp_ref.as_str())?;
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
        crate::git::pseudo_ref_guard::ensure_plain_branch_ref(repo_root, tmp_ref.as_str())?;
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
