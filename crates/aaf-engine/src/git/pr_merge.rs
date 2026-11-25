//! Native locked Pull Request merge gate (`aaf merge <PR> --expect-head <sha40>`).
//!
//! Security & correctness invariants:
//! 1. Tier-2 config is always loaded from the immutable `<base_sha>` (`git show`),
//!    tightened monotonically with Tier-1/Tier-3/env policy, never read from the PR.
//! 2. Tier-1 (`LockTier::Hard`) changes and `cross_audit.require_different_provider = true`
//!    enforce independent (`same_account = false`) approval; Tier-2 pure test changes
//!    allow solo-dev `same_account = true` approval when `require_different_provider = false`.
//! 3. Verifies `headRefOid == expect_head` both before and inside the per-repo merge lock,
//!    and passes `--match-head-commit <expect_head>` to `gh pr merge`.
//! 4. Pre-checks non-FF merges in memory via `git merge-tree --write-tree` (catching
//!    conflicts and hitting signed common receipts with zero worktree overhead);
//!    on cache miss merges `<base_sha>` + `<expect_head>` in an isolated warm slot
//!    (wiped on `Drop`) and runs the full `aaf gate` on the merged result.
//! 5. Never touches uncommitted changes or sibling worktrees in the local checkout.

use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use aaf_domain::config::{GlobalConfig, MachineConfig, RepoConfig};
use aaf_domain::gate::{GateMode, GateStatus, LockStatus, Receipt};
use aaf_domain::ports::Clock;

use super::cmd::{git, git_raw};
use super::gh_client::{GhClient, MergeStrategy, PrInfo};
use super::pr_cleanup::{PrCleanupReport, finalize_early, finish_merged};
use super::pr_merge_support::{
    InMemoryMerge, ScratchWorktree, acquire_merge_lock, bounded_deadline, check_ci,
    fetch_base_and_pr, has_local_commit_and_tree, in_memory_merge_tree, load_trusted_base_config,
    ls_remote_head, sync_local_base as sync_local_base_impl, valid_sha40,
};
use super::{GitError, has_remote};
use crate::gate::{self, HiddenPolicy};
use crate::throttle::{Progress, SystemProbe, Throttle};

const MAX_OPTIMISTIC_RETRIES: usize = 2;
/// Maximum seconds to wait for GitHub's PR view API to reflect a just-pushed
/// head commit when `--ci-wait > 0` before returning `head_mismatch`.
const HEAD_SETTLE_MAX_SECS: u64 = 15;
const HEAD_SETTLE_POLL: Duration = Duration::from_millis(500);

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
    /// Unattended merges set this: a PR with no registered check is
    /// `ci_pending`, never a pass (a just-opened PR has none yet).
    pub require_checks: bool,
    pub lock_timeout: Duration,
    pub dry_run: bool,
    pub machine: MachineConfig,
    pub global_config: Option<GlobalConfig>,
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
    let base_in_head = git_raw(
        &req.repo_root,
        &["merge-base", "--is-ancestor", base_sha, head_sha],
    )?
    .success();

    let candidate_tree = if base_in_head {
        let tree_spec = format!("{head_sha}^{{tree}}");
        git(&req.repo_root, &["rev-parse", &tree_spec]).ok()
    } else {
        match in_memory_merge_tree(&req.repo_root, base_sha, head_sha)? {
            InMemoryMerge::Conflict { detail } => {
                return Ok(VerifyOutcome::Blocked(blocked(
                    "merge_conflict",
                    format!("merge conflict with base {base_sha}: {detail}"),
                    None,
                )));
            }
            InMemoryMerge::Clean { tree_oid } => Some(tree_oid),
            InMemoryMerge::Unsupported => None,
        }
    };

    let require_independent = trusted_cfg.cross_audit.require_different_provider;
    if !trusted_cfg.proof.required
        && let Some(tree_oid) = candidate_tree
    {
        let tree_fp = format!("git-tree:{tree_oid}");
        if let Some(receipt) = gate::reuse::probe_common_receipt(
            &req.data_dir,
            &req.repo_root,
            trusted_cfg,
            req.mode,
            &tree_fp,
            base_sha,
            require_independent,
        ) {
            return Ok(VerifyOutcome::Clear(Box::new(receipt)));
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
        scratch.path(),
        &[
            // Never run hooks inside the scratch: a repo-relative core.hooksPath
            // resolves to PR-controlled files that would run before the gate.
            "-c",
            "core.hooksPath=/dev/null",
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
            git(scratch.path(), &["diff", "--name-only", "--diff-filter=U"]).unwrap_or_default();
        let _ = git_raw(scratch.path(), &["merge", "--abort"]);
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
        scratch.path(),
        trusted_cfg,
        req.mode,
        clock,
        throttle,
        gate::GateRunOptions {
            policy: HiddenPolicy::Remove,
            base_ref_override: Some(base_sha),
            require_independent_approval: require_independent,
            allow_common_receipt_write: true,
            strip_compiler_wrappers: true,
            proof_secret_allowlist: req
                .global_config
                .as_ref()
                .map(|g| g.proof.secret_env_allowlist.as_slice()),
        },
    )
    .map_err(|e| GitError::Gate(e.to_string()))?;

    let receipt = gate_run.receipt;
    if receipt.status == GateStatus::Clear {
        if let Some(reason) = crate::proof::verified_proof_needs_human_reason(
            scratch.path(),
            &req.data_dir,
            &trusted_cfg.proof,
            &receipt.tree_fp,
        ) {
            return Ok(VerifyOutcome::Blocked(blocked(
                "proof_needs_human",
                reason,
                Some(receipt),
            )));
        }
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

/// Runs the full locked PR merge gate using `gh` and `throttle`.
pub fn merge_pr(
    req: &PrMergeRequest,
    gh: &dyn GhClient,
    clock: &dyn Clock,
    progress: &dyn Fn(&Progress<'_>),
) -> Result<PrMergeOutcome, GitError> {
    merge_pr_with_cwd(req, None, gh, clock, progress).map(|(outcome, _)| outcome)
}

/// Companion to [`merge_pr`] that accepts the caller's working directory (so
/// post-merge cleanup never removes a worktree containing `caller_cwd`) and
/// returns any [`PrCleanupReport`].
pub fn merge_pr_with_cwd(
    req: &PrMergeRequest,
    caller_cwd: Option<&Path>,
    gh: &dyn GhClient,
    clock: &dyn Clock,
    progress: &dyn Fn(&Progress<'_>),
) -> Result<(PrMergeOutcome, Option<PrCleanupReport>), GitError> {
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
        return Ok(finalize_early(outcome, req, &pr, caller_cwd, false, clock));
    }
    if pr.mergeable.eq_ignore_ascii_case("CONFLICTING") {
        let reason = format!("GitHub reports PR #{} has merge conflicts", pr.number);
        return Ok((blocked("merge_conflict", reason, None), None));
    }
    let (ci_blocked, ci_waited) = check_ci(
        gh,
        &req.repo_root,
        req.pr_number,
        req.ci_wait_secs,
        req.require_checks,
    )?;
    if let Some((code, reason)) = ci_blocked {
        return Ok((blocked(&code, reason, None), None));
    }

    let common_id = gate::stats_repo_id(&req.repo_root);
    let probe = SystemProbe::detect();
    let throttle = Throttle::system(&req.machine, &probe, progress);

    // Serial per-repo merge queue (canonical lock order: MergeLock -> HeavySlot,
    // matching `merge_queue::merge`). Acquiring the repo merge lock after CI and
    // before `verify_merged_tree` prevents duplicate `aaf merge <same-PR>` calls
    // and back-to-back PRs in the same repo from burning duplicate RAM/Gradle
    // runs against a stale base. `--dry-run` stays lock-free.
    let (_lock, waited) = if req.dry_run {
        (None, false)
    } else {
        let locks_dir = req.data_dir.join("locks");
        match acquire_merge_lock(
            &locks_dir,
            &common_id,
            req.pr_number,
            req.lock_timeout,
            progress,
        )? {
            Some((guard, waited)) => (Some(guard), waited),
            None => {
                // One last check in case the active merger just landed this PR.
                if let Ok(pr_now) = gh.view_pr(&req.repo_root, req.pr_number)
                    && pr_now.state.eq_ignore_ascii_case("MERGED")
                {
                    let out = PrMergeOutcome::AlreadyMerged {
                        pr_number: pr_now.number,
                        url: pr_now.url.clone(),
                    };
                    return Ok(finalize_early(out, req, &pr_now, caller_cwd, false, clock));
                }
                let reason = format!(
                    "another merge in this repository held the merge lock for {}s",
                    req.lock_timeout.as_secs()
                );
                return Ok((blocked("merge_in_progress", reason, None), None));
            }
        }
    };
    // Re-check PR state only when we actually waited on another merge or slept
    // waiting for pending CI checks: avoids an extra GitHub API round-trip on
    // immediate uncontended merges while still catching AlreadyMerged / draft /
    // head changes after waiting.
    if waited || ci_waited {
        let pr_after_wait = gh.view_pr(&req.repo_root, req.pr_number)?;
        if let Some(outcome) = check_pr_open_and_head(&pr_after_wait, &req.expect_head, false, None)
        {
            return Ok(finalize_early(
                outcome,
                req,
                &pr_after_wait,
                caller_cwd,
                _lock.is_some(),
                clock,
            ));
        }
        pr = pr_after_wait;
    }

    let mut attempt = 0usize;
    loop {
        let (base_sha, head_sha) =
            fetch_base_and_pr(&req.repo_root, &req.remote, &pr, &req.expect_head)?;
        let trusted_cfg =
            load_trusted_base_config(&req.repo_root, &base_sha, req.global_config.as_ref())?;
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
            VerifyOutcome::Blocked(outcome) => return Ok((outcome, None)),
        };

        // Phase B3b: parallel post-gate refresh (`gh.view_pr` + `ls_remote_head`).
        let (pr_now_res, remote_base_res) = thread::scope(|scope| {
            let ls_handle =
                scope.spawn(|| ls_remote_head(&req.repo_root, &req.remote, &pr.base_ref_name));
            let pr_res = gh.view_pr(&req.repo_root, req.pr_number);
            let ls_res = ls_handle.join().unwrap_or_else(|_| {
                Err(GitError::Invalid("ls-remote worker panicked".to_string()))
            });
            (pr_res, ls_res)
        });
        let pr_now = pr_now_res?;
        if let Some(outcome) =
            check_pr_open_and_head(&pr_now, &req.expect_head, true, Some(&receipt))
        {
            return Ok(finalize_early(
                outcome,
                req,
                &pr_now,
                caller_cwd,
                _lock.is_some(),
                clock,
            ));
        }
        let remote_base_sha = remote_base_res.ok().flatten();
        let can_skip_fetch = pr_now.state.eq_ignore_ascii_case("OPEN")
            && pr_now.head_ref_oid.eq_ignore_ascii_case(&req.expect_head)
            && pr_now.base_ref_name == pr.base_ref_name
            && remote_base_sha
                .as_deref()
                .is_some_and(|sha| sha.eq_ignore_ascii_case(&base_sha))
            && has_local_commit_and_tree(&req.repo_root, &req.expect_head)?;
        let (base_now, head_now) = if can_skip_fetch {
            (base_sha.clone(), head_sha.clone())
        } else {
            fetch_base_and_pr(&req.repo_root, &req.remote, &pr_now, &req.expect_head)?
        };

        let (final_receipt, final_cfg) = if base_now == base_sha {
            (receipt, trusted_cfg)
        } else if attempt < MAX_OPTIMISTIC_RETRIES {
            // An external actor (e.g. GitHub Web UI) moved base while we verified;
            // re-verify cleanly while keeping the repo merge lock held.
            attempt += 1;
            pr = pr_now;
            continue;
        } else {
            let cfg_now =
                load_trusted_base_config(&req.repo_root, &base_now, req.global_config.as_ref())?;
            match verify_merged_tree(
                req, &common_id, &base_now, &head_now, &cfg_now, clock, &throttle,
            )? {
                VerifyOutcome::Clear(r) => (*r, cfg_now),
                VerifyOutcome::Blocked(outcome) => return Ok((outcome, None)),
            }
        };

        let need_independent = final_receipt.head_lock.has_hard_lock()
            || final_cfg.cross_audit.require_different_provider;
        if final_receipt.head_lock.status == LockStatus::Approved
            && need_independent
            && !final_receipt
                .head_lock
                .approval
                .as_ref()
                .is_some_and(|a| !a.same_account)
        {
            let reason = "head-locked changes require an independent (different-provider) approval"
                .to_string();
            return Ok((
                blocked("cross_approval_required", reason, Some(final_receipt)),
                None,
            ));
        }

        if req.dry_run {
            return Ok((
                PrMergeOutcome::DryRunClear {
                    pr_number: pr_now.number,
                    head_sha: head_now,
                    base_sha: base_now,
                    receipt: Box::new(final_receipt),
                },
                None,
            ));
        }

        gh.merge_pr(
            &req.repo_root,
            pr_now.number,
            &req.expect_head,
            req.strategy,
            req.subject.as_deref(),
            req.body_file.as_deref(),
        )?;
        let (local_sync_note, cleanup) = finish_merged(req, &pr_now, caller_cwd, clock);
        return Ok((
            PrMergeOutcome::Merged {
                pr_number: pr_now.number,
                head_sha: head_now,
                base_sha: base_now,
                receipt: Box::new(final_receipt),
                local_sync_note,
            },
            Some(cleanup),
        ));
    }
}

/// Fetches `remote/base_branch` and fast-forwards local `refs/heads/<base_branch>`.
pub fn sync_local_base(repo_root: &Path, remote: &str, base_branch: &str) -> String {
    sync_local_base_impl(repo_root, remote, base_branch)
}
