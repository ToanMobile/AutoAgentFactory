//! Native locked Pull Request merge gate (`aaf merge <PR> --expect-head <sha40>`).
//!
//! Security & correctness invariants:
//! 1. Tier-2 config is always loaded from the immutable `<base_sha>` (`git show`), never from the PR.
//! 2. `cross_audit.require_different_provider` is enforced unconditionally (`same_account=true`
//!    approvals never unlock head-locked changes during merge).
//! 3. Verifies `headRefOid == expect_head` both before and inside the per-repo merge lock,
//!    and passes `--match-head-commit <expect_head>` to `gh pr merge`.
//! 4. Merges `<base_sha>` + `<expect_head>` in an isolated detached scratch worktree
//!    (cleaned up on `Drop`) and runs the full `aaf gate` on the merged result.
//! 5. Never touches uncommitted changes or sibling worktrees in the local checkout.

use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use aaf_domain::config::{MachineConfig, RepoConfig};
use aaf_domain::gate::{GateMode, GateStatus, LockStatus, Receipt};
use aaf_domain::ports::Clock;

use super::cmd::{git, git_raw};
use super::gh_client::{GhClient, MergeStrategy, PrInfo};
use super::pr_merge_support::{
    MAX_WAIT_SECS, MergeLockGuard, ScratchWorktree, bounded_deadline, fetch_base_and_pr,
};
use super::{GitError, has_remote};
use crate::gate::{self, HiddenPolicy};
use crate::throttle::{Progress, SystemProbe, Throttle};

const MAX_OPTIMISTIC_RETRIES: usize = 2;
/// Maximum seconds to wait for GitHub's PR view API to reflect a just-pushed
/// head commit when `--ci-wait > 0` before returning `head_mismatch`.
const HEAD_SETTLE_MAX_SECS: u64 = 15;
const HEAD_SETTLE_POLL: Duration = Duration::from_millis(500);
/// How often a waiter on the per-repo merge lock reports progress.
const MERGE_LOCK_PROGRESS_SECS: u64 = 15;

#[derive(Debug, Clone)]
pub struct PrMergeRequest {
    pub repo_root: PathBuf,
    pub data_dir: PathBuf,
    pub pr_number: u64,
    pub expect_head: String,
    pub remote: String,
    pub mode: GateMode,
    pub strategy: MergeStrategy,
    pub subject: Option<String>,
    pub body_file: Option<PathBuf>,
    pub ci_wait_secs: u64,
    pub lock_timeout: Duration,
    pub dry_run: bool,
    pub machine: MachineConfig,
}

#[derive(Debug, Clone)]
pub enum PrMergeOutcome {
    Merged {
        pr_number: u64,
        head_sha: String,
        base_sha: String,
        receipt: Box<Receipt>,
        local_sync_note: String,
    },
    DryRunClear {
        pr_number: u64,
        head_sha: String,
        base_sha: String,
        receipt: Box<Receipt>,
    },
    AlreadyMerged {
        pr_number: u64,
        url: String,
    },
    Blocked {
        code: String,
        reason: String,
        receipt: Option<Box<Receipt>>,
    },
}

enum VerifyOutcome {
    Clear(Box<Receipt>),
    Blocked(PrMergeOutcome),
}

fn blocked(code: &str, reason: String, receipt: Option<Receipt>) -> PrMergeOutcome {
    PrMergeOutcome::Blocked {
        code: code.to_string(),
        reason,
        receipt: receipt.map(Box::new),
    }
}

fn valid_sha40(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn load_trusted_base_config(repo_root: &Path, base_sha: &str) -> Result<RepoConfig, GitError> {
    let spec = format!("{base_sha}:.autofactory/config.toml");
    let raw = git(repo_root, &["show", &spec]).map_err(|_| {
        GitError::Gate(format!(
            "missing or unreadable .autofactory/config.toml at base {base_sha}"
        ))
    })?;
    toml::from_str(&raw).map_err(|e| {
        GitError::Gate(format!(
            "invalid .autofactory/config.toml at base {base_sha}: {e}"
        ))
    })
}

fn check_ci(
    gh: &dyn GhClient,
    repo_root: &Path,
    pr_number: u64,
    wait_secs: u64,
) -> Result<Option<(String, String)>, GitError> {
    let deadline = bounded_deadline(Duration::from_secs(wait_secs.min(MAX_WAIT_SECS)));
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

/// Acquires the per-repo merge lock with private permissions, bounded deadline,
/// and periodic `WaitingForMergeLock` progress reports. Returns `Ok(None)` on timeout.
fn acquire_merge_lock(
    locks_dir: &Path,
    common_id: &str,
    pr_number: u64,
    timeout: Duration,
    progress: &dyn Fn(&Progress<'_>),
) -> Result<Option<(MergeLockGuard, bool)>, GitError> {
    crate::db::private::create_dir(locks_dir)?;
    let lock_path = locks_dir.join(format!("{common_id}.merge.lock"));
    drop(crate::db::private::open_file(&lock_path)?);
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

fn check_pr_open_and_head(
    pr: &PrInfo,
    expect_head: &str,
    after_gate: bool,
    receipt: Option<&Receipt>,
) -> Option<PrMergeOutcome> {
    if pr.state.eq_ignore_ascii_case("MERGED") {
        return Some(PrMergeOutcome::AlreadyMerged {
            pr_number: pr.number,
            url: pr.url.clone(),
        });
    }
    if !pr.state.eq_ignore_ascii_case("OPEN") {
        return Some(blocked(
            "pr_not_open",
            format!("PR #{} is in state {}", pr.number, pr.state),
            receipt.cloned(),
        ));
    }
    if pr.is_draft {
        return Some(blocked(
            "pr_is_draft",
            format!(
                "PR #{} is a draft; mark it ready for review first",
                pr.number
            ),
            receipt.cloned(),
        ));
    }
    if !pr.head_ref_oid.eq_ignore_ascii_case(expect_head) {
        let (code, reason) = if after_gate {
            (
                "head_moved",
                format!(
                    "PR #{} head moved during gate to {} (expected {expect_head})",
                    pr.number, pr.head_ref_oid
                ),
            )
        } else {
            (
                "head_mismatch",
                format!(
                    "PR #{} head is {}, expected {expect_head}",
                    pr.number, pr.head_ref_oid
                ),
            )
        };
        return Some(blocked(code, reason, receipt.cloned()));
    }
    None
}

fn verify_merged_tree(
    req: &PrMergeRequest,
    common_id: &str,
    base_sha: &str,
    head_sha: &str,
    trusted_cfg: &RepoConfig,
    clock: &dyn Clock,
    throttle: &Throttle<'_>,
) -> Result<VerifyOutcome, GitError> {
    let head_in_base = git_raw(
        &req.repo_root,
        &["merge-base", "--is-ancestor", head_sha, base_sha],
    )?;
    if head_in_base.success() {
        return Ok(VerifyOutcome::Blocked(blocked(
            "empty_pr",
            "PR head is already an ancestor of base; nothing to merge".to_string(),
            None,
        )));
    }
    if !trusted_cfg.proof.required {
        let base_in_head = git_raw(
            &req.repo_root,
            &["merge-base", "--is-ancestor", base_sha, head_sha],
        )?;
        let tree_spec = format!("{head_sha}^{{tree}}");
        if base_in_head.success()
            && let Ok(head_tree) = git(&req.repo_root, &["rev-parse", &tree_spec])
        {
            let tree_fp = format!("git-tree:{head_tree}");
            if let Some(receipt) = gate::reuse::probe_common_receipt(
                &req.data_dir,
                &req.repo_root,
                trusted_cfg,
                req.mode,
                &tree_fp,
                base_sha,
                true,
            ) {
                return Ok(VerifyOutcome::Clear(Box::new(receipt)));
            }
        }
    }
    let scratch = ScratchWorktree::create(
        &req.repo_root,
        &req.data_dir,
        common_id,
        req.pr_number,
        base_sha,
    )?;
    let merge_res = git_raw(
        &scratch.path,
        &[
            "-c",
            "user.name=AutoAgentFactory",
            "-c",
            "user.email=aaf@local",
            "-c",
            "commit.gpgsign=false",
            "merge",
            "--no-ff",
            "--no-edit",
            "--no-verify",
            "-m",
            "aaf merge verify",
            head_sha,
        ],
    )?;
    if !merge_res.success() {
        let conflicts =
            git(&scratch.path, &["diff", "--name-only", "--diff-filter=U"]).unwrap_or_default();
        let _ = git_raw(&scratch.path, &["merge", "--abort"]);
        let reason = format!(
            "merge conflict with base {base_sha}: {}",
            conflicts.lines().collect::<Vec<_>>().join(", ")
        );
        return Ok(VerifyOutcome::Blocked(blocked(
            "merge_conflict",
            reason,
            None,
        )));
    }
    let gate_run = gate::run_gate_with_options(
        &req.data_dir,
        &scratch.path,
        trusted_cfg,
        req.mode,
        clock,
        throttle,
        gate::GateRunOptions {
            policy: HiddenPolicy::Remove,
            base_ref_override: Some(base_sha),
            require_independent_approval: true,
        },
    )
    .map_err(|e| GitError::Gate(e.to_string()))?;

    let receipt = gate_run.receipt;
    if receipt.status == GateStatus::Clear {
        Ok(VerifyOutcome::Clear(Box::new(receipt)))
    } else {
        let code = if receipt.head_lock.status == LockStatus::Blocked {
            "cross_approval_required"
        } else {
            "gate_blocked"
        };
        let reason = receipt.reasons.join("; ");
        Ok(VerifyOutcome::Blocked(blocked(code, reason, Some(receipt))))
    }
}

fn sync_local_base(repo_root: &Path, remote: &str, base_branch: &str) -> String {
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
    match git_raw(
        repo_root,
        &["merge", "--ff-only", "--no-overwrite-ignore", &target],
    ) {
        Ok(st) if st.success() => format!("fast-forwarded local `{base_branch}` to `{target}`"),
        _ => format!(
            "warning: local `{base_branch}` could not fast-forward to `{target}` (diverged)"
        ),
    }
}

/// Runs the full locked PR merge gate using `gh` and `throttle`.
pub fn merge_pr(
    req: &PrMergeRequest,
    gh: &dyn GhClient,
    clock: &dyn Clock,
    progress: &dyn Fn(&Progress<'_>),
) -> Result<PrMergeOutcome, GitError> {
    if req.pr_number == 0 {
        return Err(GitError::Invalid("PR number must be > 0".to_string()));
    }
    if !valid_sha40(&req.expect_head) {
        return Err(GitError::Invalid(format!(
            "--expect-head must be a 40-char hex commit SHA, got {:?}",
            req.expect_head
        )));
    }
    if !has_remote(&req.repo_root, &req.remote)? {
        return Err(GitError::Invalid(format!(
            "remote {:?} is not configured in {}",
            req.remote,
            req.repo_root.display()
        )));
    }
    let mut pr = gh.view_pr(&req.repo_root, req.pr_number)?;
    // Right after `git push`, GitHub's PR API can briefly report the previous
    // head SHA. When the caller opted into waiting (`ci_wait_secs > 0`), poll
    // for a short bounded settle window before failing closed with `head_mismatch`.
    if pr.state.eq_ignore_ascii_case("OPEN")
        && !pr.is_draft
        && !pr.head_ref_oid.eq_ignore_ascii_case(&req.expect_head)
        && req.ci_wait_secs > 0
    {
        let settle = Duration::from_secs(req.ci_wait_secs.min(HEAD_SETTLE_MAX_SECS));
        let deadline = bounded_deadline(settle);
        while Instant::now() < deadline {
            let rem = deadline.saturating_duration_since(Instant::now());
            thread::sleep(rem.min(HEAD_SETTLE_POLL));
            pr = gh.view_pr(&req.repo_root, req.pr_number)?;
            if !pr.state.eq_ignore_ascii_case("OPEN")
                || pr.is_draft
                || pr.head_ref_oid.eq_ignore_ascii_case(&req.expect_head)
            {
                break;
            }
        }
    }
    if let Some(outcome) = check_pr_open_and_head(&pr, &req.expect_head, false, None) {
        return Ok(outcome);
    }
    if pr.mergeable.eq_ignore_ascii_case("CONFLICTING") {
        return Ok(PrMergeOutcome::Blocked {
            code: "merge_conflict".to_string(),
            reason: format!("GitHub reports PR #{} has merge conflicts", pr.number),
            receipt: None,
        });
    }
    if let Some((code, reason)) = check_ci(gh, &req.repo_root, req.pr_number, req.ci_wait_secs)? {
        return Ok(PrMergeOutcome::Blocked {
            code,
            reason,
            receipt: None,
        });
    }

    let common_id = gate::stats_repo_id(&req.repo_root);
    let probe = SystemProbe::detect();
    let throttle = Throttle::system(&req.machine, &probe, progress);

    // Serial per-repo merge queue (canonical lock order: MergeLock -> HeavySlot,
    // matching `merge_queue::merge`). Acquiring the repo merge lock after CI and
    // before `verify_merged_tree` prevents duplicate `aaf merge <same-PR>` calls
    // and back-to-back PRs in the same repo from burning duplicate RAM/Gradle
    // runs against a stale base. `--dry-run` stays lock-free.
    let _lock = if req.dry_run {
        None
    } else {
        let locks_dir = req.data_dir.join("locks");
        match acquire_merge_lock(
            &locks_dir,
            &common_id,
            req.pr_number,
            req.lock_timeout,
            progress,
        )? {
            Some((guard, waited)) => {
                // Re-check PR state only when we actually waited on another merge:
                // avoids an extra GitHub API round-trip on uncontended merges while
                // still catching AlreadyMerged / draft / head changes after waiting.
                if waited {
                    let pr_after_lock = gh.view_pr(&req.repo_root, req.pr_number)?;
                    if let Some(outcome) =
                        check_pr_open_and_head(&pr_after_lock, &req.expect_head, false, None)
                    {
                        return Ok(outcome);
                    }
                    pr = pr_after_lock;
                }
                Some(guard)
            }
            None => {
                // One last check in case the active merger just landed this PR.
                if let Ok(pr_now) = gh.view_pr(&req.repo_root, req.pr_number)
                    && pr_now.state.eq_ignore_ascii_case("MERGED")
                {
                    return Ok(PrMergeOutcome::AlreadyMerged {
                        pr_number: pr_now.number,
                        url: pr_now.url,
                    });
                }
                return Ok(PrMergeOutcome::Blocked {
                    code: "merge_in_progress".to_string(),
                    reason: format!(
                        "another merge in this repository held the merge lock for {}s",
                        req.lock_timeout.as_secs()
                    ),
                    receipt: None,
                });
            }
        }
    };

    let mut attempt = 0usize;
    loop {
        let (base_sha, head_sha) =
            fetch_base_and_pr(&req.repo_root, &req.remote, &pr, &req.expect_head)?;
        let trusted_cfg = load_trusted_base_config(&req.repo_root, &base_sha)?;
        let receipt = match verify_merged_tree(
            req,
            &common_id,
            &base_sha,
            &head_sha,
            &trusted_cfg,
            clock,
            &throttle,
        )? {
            VerifyOutcome::Clear(r) => *r,
            VerifyOutcome::Blocked(outcome) => return Ok(outcome),
        };

        let pr_now = gh.view_pr(&req.repo_root, req.pr_number)?;
        if let Some(outcome) =
            check_pr_open_and_head(&pr_now, &req.expect_head, true, Some(&receipt))
        {
            return Ok(outcome);
        }
        // `fetch_base_and_pr` skips the PR-head network fetch when GitHub still
        // reports `expect_head` and the commit + tree already exist locally.
        let (base_now, head_now) =
            fetch_base_and_pr(&req.repo_root, &req.remote, &pr_now, &req.expect_head)?;
        let final_receipt = if base_now == base_sha {
            receipt
        } else if attempt < MAX_OPTIMISTIC_RETRIES {
            // An external actor (e.g. GitHub Web UI) moved base while we verified;
            // re-verify cleanly while keeping the repo merge lock held.
            attempt += 1;
            pr = pr_now;
            continue;
        } else {
            let cfg_now = load_trusted_base_config(&req.repo_root, &base_now)?;
            match verify_merged_tree(
                req, &common_id, &base_now, &head_now, &cfg_now, clock, &throttle,
            )? {
                VerifyOutcome::Clear(r) => *r,
                VerifyOutcome::Blocked(outcome) => return Ok(outcome),
            }
        };

        if final_receipt.head_lock.status == LockStatus::Approved
            && !final_receipt
                .head_lock
                .approval
                .as_ref()
                .is_some_and(|a| !a.same_account)
        {
            return Ok(PrMergeOutcome::Blocked {
                code: "cross_approval_required".to_string(),
                reason: "head-locked changes require an independent (different-provider) approval"
                    .to_string(),
                receipt: Some(Box::new(final_receipt)),
            });
        }

        if req.dry_run {
            return Ok(PrMergeOutcome::DryRunClear {
                pr_number: pr_now.number,
                head_sha: head_now,
                base_sha: base_now,
                receipt: Box::new(final_receipt),
            });
        }

        gh.merge_pr(
            &req.repo_root,
            pr_now.number,
            &req.expect_head,
            req.strategy,
            req.subject.as_deref(),
            req.body_file.as_deref(),
        )?;
        let local_sync_note = sync_local_base(&req.repo_root, &req.remote, &pr_now.base_ref_name);
        return Ok(PrMergeOutcome::Merged {
            pr_number: pr_now.number,
            head_sha: head_now,
            base_sha: base_now,
            receipt: Box::new(final_receipt),
            local_sync_note,
        });
    }
}
