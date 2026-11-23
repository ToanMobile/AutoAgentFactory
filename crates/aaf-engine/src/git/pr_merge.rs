//! Native locked Pull Request merge gate (`aaf merge <PR> --expect-head <sha40>`).
//!
//! Security & correctness invariants:
//! 1. Tier-2 config is always loaded from `origin/<base>` (`git show`), never from the PR.
//! 2. `cross_audit.require_different_provider` is enforced unconditionally (`same_account=true`
//!    approvals never unlock head-locked changes during merge).
//! 3. Verifies `headRefOid == expect_head` both before and inside the per-repo merge lock,
//!    and passes `--match-head-commit <expect_head>` to `gh pr merge`.
//! 4. Merges `origin/<base>` + `<expect_head>` in an isolated detached scratch worktree
//!    (cleaned up on `Drop`) and runs the full `aaf gate` on the merged result.
//! 5. Never touches uncommitted changes or sibling worktrees in the local checkout.

use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use aaf_domain::config::{MachineConfig, RepoConfig};
use aaf_domain::gate::{GateMode, GateStatus, Receipt};
use aaf_domain::ports::Clock;

use super::cmd::{git, git_raw};
use super::gh_client::{GhClient, MergeStrategy, PrInfo};
use super::{GitError, check_branch_name, path_str};
use crate::gate::{self, HiddenPolicy};
use crate::throttle::{Progress, SystemProbe, Throttle};

const MAX_OPTIMISTIC_RETRIES: usize = 2;
static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

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
    Clear(Receipt),
    Blocked {
        code: String,
        reason: String,
        receipt: Option<Receipt>,
    },
}

fn valid_sha40(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

struct ScratchWorktree {
    repo_root: PathBuf,
    path: PathBuf,
}

impl ScratchWorktree {
    fn create(
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

struct MergeLockGuard(File);

impl MergeLockGuard {
    fn acquire(locks_dir: &Path, common_id: &str, timeout: Duration) -> Result<Self, GitError> {
        fs::create_dir_all(locks_dir)?;
        let path = locks_dir.join(format!("{common_id}.merge.lock"));
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)?;
        let deadline = Instant::now() + timeout;
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

fn load_trusted_base_config(
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

fn check_ci(
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

fn fetch_base_and_pr(
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
    if !git_raw(repo_root, &["fetch", "--quiet", remote, &pr_refspec])?.success() {
        check_branch_name(repo_root, &pr.head_ref_name)?;
        let branch_refspec = format!("+refs/heads/{}:{tmp_ref}", pr.head_ref_name);
        git(repo_root, &["fetch", "--quiet", remote, &branch_refspec])?;
    }
    let fetched_head = git(repo_root, &["rev-parse", &tmp_ref])?;
    let _ = git_raw(repo_root, &["update-ref", "-d", &tmp_ref]);
    if !fetched_head.eq_ignore_ascii_case(expect_head) {
        return Err(GitError::Invalid(format!(
            "fetched PR head {fetched_head} does not match --expect-head {expect_head}"
        )));
    }
    Ok((base_sha, fetched_head))
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
    let scratch = ScratchWorktree::create(
        &req.repo_root,
        &req.data_dir,
        common_id,
        req.pr_number,
        base_sha,
    )?;
    let ancestor = git_raw(
        &scratch.path,
        &["merge-base", "--is-ancestor", head_sha, base_sha],
    )?;
    if ancestor.success() {
        return Ok(VerifyOutcome::Blocked {
            code: "empty_pr".to_string(),
            reason: "PR head is already an ancestor of base; nothing to merge".to_string(),
            receipt: None,
        });
    }
    let merge_res = git_raw(
        &scratch.path,
        &[
            "-c",
            "user.name=AutoAgentFactory",
            "-c",
            "user.email=aaf@local",
            "merge",
            "--no-commit",
            "--no-ff",
            head_sha,
        ],
    )?;
    if !merge_res.success() {
        let conflicts =
            git(&scratch.path, &["diff", "--name-only", "--diff-filter=U"]).unwrap_or_default();
        let _ = git_raw(&scratch.path, &["merge", "--abort"]);
        return Ok(VerifyOutcome::Blocked {
            code: "merge_conflict".to_string(),
            reason: format!(
                "merge conflict with base {base_sha}: {}",
                conflicts.lines().collect::<Vec<_>>().join(", ")
            ),
            receipt: None,
        });
    }
    let _ = git_raw(&scratch.path, &["reset", "--mixed", "HEAD"]);
    let gate_run = gate::run_gate_policy(
        &req.data_dir,
        &scratch.path,
        trusted_cfg,
        req.mode,
        clock,
        throttle,
        HiddenPolicy::Remove,
    )
    .map_err(|e| GitError::Gate(e.to_string()))?;

    let receipt = gate_run.receipt;
    if receipt.status == GateStatus::Clear {
        Ok(VerifyOutcome::Clear(receipt))
    } else {
        let code = if receipt.head_lock.status == aaf_domain::gate::LockStatus::Blocked {
            "cross_approval_required"
        } else {
            "gate_blocked"
        };
        let reason = receipt.reasons.join("; ");
        Ok(VerifyOutcome::Blocked {
            code: code.to_string(),
            reason,
            receipt: Some(receipt),
        })
    }
}

fn sync_local_base(repo_root: &Path, remote: &str, base_branch: &str) -> String {
    let refspec = format!("+refs/heads/{base_branch}:refs/remotes/{remote}/{base_branch}");
    if git(repo_root, &["fetch", "--quiet", remote, &refspec]).is_err() {
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
    let pr = gh.view_pr(&req.repo_root, req.pr_number)?;
    if pr.state.eq_ignore_ascii_case("MERGED") {
        return Ok(PrMergeOutcome::AlreadyMerged {
            pr_number: pr.number,
            url: pr.url,
        });
    }
    if !pr.state.eq_ignore_ascii_case("OPEN") {
        return Ok(PrMergeOutcome::Blocked {
            code: "pr_not_open".to_string(),
            reason: format!("PR #{} is in state {}", pr.number, pr.state),
            receipt: None,
        });
    }
    if pr.is_draft {
        return Ok(PrMergeOutcome::Blocked {
            code: "pr_is_draft".to_string(),
            reason: format!(
                "PR #{} is a draft; mark it ready for review first",
                pr.number
            ),
            receipt: None,
        });
    }
    if !pr.head_ref_oid.eq_ignore_ascii_case(&req.expect_head) {
        return Ok(PrMergeOutcome::Blocked {
            code: "head_mismatch".to_string(),
            reason: format!(
                "PR #{} head is {}, expected {}",
                pr.number, pr.head_ref_oid, req.expect_head
            ),
            receipt: None,
        });
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

    let mut attempt = 0usize;
    loop {
        let (base_sha, head_sha) =
            fetch_base_and_pr(&req.repo_root, &req.remote, &pr, &req.expect_head)?;
        let trusted_cfg = load_trusted_base_config(&req.repo_root, &req.remote, &pr.base_ref_name)?;
        let receipt = match verify_merged_tree(
            req,
            &common_id,
            &base_sha,
            &head_sha,
            &trusted_cfg,
            clock,
            &throttle,
        )? {
            VerifyOutcome::Clear(r) => r,
            VerifyOutcome::Blocked {
                code,
                reason,
                receipt,
            } => {
                return Ok(PrMergeOutcome::Blocked {
                    code,
                    reason,
                    receipt: receipt.map(Box::new),
                });
            }
        };

        let locks_dir = req.data_dir.join("locks");
        let _lock = MergeLockGuard::acquire(&locks_dir, &common_id, req.lock_timeout)?;

        let pr_now = gh.view_pr(&req.repo_root, req.pr_number)?;
        if pr_now.state.eq_ignore_ascii_case("MERGED") {
            return Ok(PrMergeOutcome::AlreadyMerged {
                pr_number: pr_now.number,
                url: pr_now.url,
            });
        }
        if !pr_now.head_ref_oid.eq_ignore_ascii_case(&req.expect_head) {
            return Ok(PrMergeOutcome::Blocked {
                code: "head_moved".to_string(),
                reason: format!(
                    "PR #{} head moved during gate to {} (expected {})",
                    pr_now.number, pr_now.head_ref_oid, req.expect_head
                ),
                receipt: Some(Box::new(receipt)),
            });
        }
        let (base_now, head_now) =
            fetch_base_and_pr(&req.repo_root, &req.remote, &pr_now, &req.expect_head)?;
        let final_receipt = if base_now == base_sha {
            receipt
        } else if attempt < MAX_OPTIMISTIC_RETRIES {
            attempt += 1;
            drop(_lock);
            continue;
        } else {
            let cfg_now =
                load_trusted_base_config(&req.repo_root, &req.remote, &pr_now.base_ref_name)?;
            match verify_merged_tree(
                req, &common_id, &base_now, &head_now, &cfg_now, clock, &throttle,
            )? {
                VerifyOutcome::Clear(r) => r,
                VerifyOutcome::Blocked {
                    code,
                    reason,
                    receipt: r,
                } => {
                    return Ok(PrMergeOutcome::Blocked {
                        code,
                        reason,
                        receipt: r.map(Box::new),
                    });
                }
            }
        };

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
