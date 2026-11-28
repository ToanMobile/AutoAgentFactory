//! Verification of the merged tree for `aaf merge`: the base and the PR head
//! merged in a scratch worktree and gated there, or a signed receipt that
//! already proves that exact tree.

use aaf_domain::config::RepoConfig;
use aaf_domain::gate::{CommandResult, GateStatus, LockStatus, Receipt, StoreStatus};
use aaf_domain::ports::Clock;

use super::pr_merge::{PrMergeOutcome, PrMergeRequest, blocked};
use super::pr_merge_support::{InMemoryMerge, ScratchWorktree, in_memory_merge_tree};
use crate::gate::{self, HiddenPolicy};
use crate::git::GitError;
use crate::git::cmd::{git, git_raw};
use crate::throttle::Throttle;

pub(super) enum VerifyOutcome {
    Clear(Box<Receipt>),
    /// Everything passed but the head lock (see [`pending_head_lock_only`]).
    PendingHeadLock(Box<Receipt>),
    Blocked(PrMergeOutcome),
}

/// True only when the head lock is the sole block and the gate really ran: the
/// gate said so itself ([`gate::HEAD_LOCK_ONLY_NOTE`], written only when the
/// verdict that left the lock out was `Clear`: commands, proof and oracle all
/// passed), the lock is blocked and fingerprinted, at least one command ran
/// and every command passed, and none was skipped. Deliberately stricter than
/// the factory's head-lock-only test, which counts "no commands" as passing.
pub(crate) fn pending_head_lock_only(receipt: &Receipt) -> bool {
    receipt.status == GateStatus::Blocked
        && receipt.head_lock.status == LockStatus::Blocked
        && !receipt.head_lock.lock_fp.is_empty()
        && receipt.store_compliance.status != StoreStatus::Blocked
        && !receipt.commands.is_empty()
        && receipt.commands.iter().all(CommandResult::passed)
        && receipt
            .reasons
            .iter()
            .any(|r| r == gate::HEAD_LOCK_ONLY_NOTE)
        && !receipt
            .reasons
            .iter()
            .any(|r| r.starts_with("commands skipped:"))
}

pub(super) fn verify_merged_tree(
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
    // Only a dry run may look past the head lock to run the commands.
    let despite_head_lock = req.dry_run && req.run_commands_despite_head_lock;
    let no_reuse = req.no_receipt_reuse || despite_head_lock;
    if !trusted_cfg.proof.required
        && !no_reuse
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
            allow_common_receipt_write: !no_reuse,
            strip_compiler_wrappers: true,
            proof_secret_allowlist: req
                .global_config
                .as_ref()
                .map(|g| g.proof.secret_env_allowlist.as_slice()),
            no_receipt_reuse: req.no_receipt_reuse,
            run_commands_despite_head_lock: despite_head_lock,
        },
    )
    .map_err(|e| GitError::Gate(e.to_string()))?;

    let receipt = gate_run.receipt;
    // Ahead of `cross_approval_required`: a lock that is the only block of a
    // dry run whose commands all passed is not yet a refusal.
    let pending_lock = despite_head_lock && pending_head_lock_only(&receipt);
    if receipt.status == GateStatus::Clear || pending_lock {
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
        if pending_lock {
            return Ok(VerifyOutcome::PendingHeadLock(Box::new(receipt)));
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use aaf_domain::gate::{
        GateMode, HeadLockReport, RECEIPT_SCHEMA_VERSION, StoreComplianceReport,
    };

    use super::*;

    fn passing() -> CommandResult {
        CommandResult {
            name: "test_scoped".to_string(),
            command: "true".to_string(),
            exit_code: Some(0),
            timed_out: false,
            duration_ms: 1,
            log_tail: String::new(),
        }
    }

    /// What the gate writes when the head lock is the only block of a run.
    fn pending() -> Receipt {
        Receipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            repo_root: "/r".to_string(),
            tree_fp: "git-tree:x".to_string(),
            head_sha: None,
            base_sha: None,
            mode: GateMode::Scoped,
            status: GateStatus::Blocked,
            reasons: vec![
                "head lock: 1 locked file(s) changed without a signed approval".to_string(),
                gate::HEAD_LOCK_ONLY_NOTE.to_string(),
                "oracle: red_green=proven".to_string(),
                "affected packages: 1 of the workspace (a)".to_string(),
            ],
            config_hash: String::new(),
            commands: vec![passing()],
            static_findings: Vec::new(),
            suppressions: 0,
            head_lock: HeadLockReport {
                status: LockStatus::Blocked,
                lock_fp: "fp".to_string(),
                locked_changes: Vec::new(),
                approval: None,
            },
            store_compliance: StoreComplianceReport::not_applicable(),
            created_at: 0,
        }
    }

    #[test]
    fn a_lock_the_gate_called_the_only_block_is_pending_whatever_notes_sit_beside_it() {
        assert!(pending_head_lock_only(&pending()));
    }

    #[test]
    fn without_the_gates_own_note_nothing_is_pending() {
        // E.g. the lock plus a reason the verdict did not leave out.
        let mut r = pending();
        r.reasons.retain(|x| x != gate::HEAD_LOCK_ONLY_NOTE);
        assert!(!pending_head_lock_only(&r));
        let mut plain = pending();
        plain.reasons =
            vec!["head lock: 1 locked file(s) changed without a signed approval".into()];
        assert!(!pending_head_lock_only(&plain));
    }

    #[test]
    fn no_commands_is_not_pending() {
        let mut r = pending();
        r.commands.clear();
        assert!(!pending_head_lock_only(&r));
    }

    #[test]
    fn a_failing_or_timed_out_command_is_not_pending() {
        let mut r = pending();
        r.commands[0].exit_code = Some(1);
        assert!(!pending_head_lock_only(&r));
        let mut r = pending();
        r.commands[0].timed_out = true;
        assert!(!pending_head_lock_only(&r));
    }

    #[test]
    fn skipped_commands_are_not_pending_even_beside_the_note() {
        let mut r = pending();
        r.reasons
            .push("commands skipped: an earlier stage already blocks".to_string());
        assert!(!pending_head_lock_only(&r));
    }

    #[test]
    fn it_needs_a_blocked_lock_with_a_fingerprint_and_a_blocked_receipt() {
        let mut r = pending();
        r.head_lock.lock_fp.clear();
        assert!(!pending_head_lock_only(&r));
        let mut r = pending();
        r.head_lock.status = LockStatus::Approved;
        assert!(!pending_head_lock_only(&r));
        let mut r = pending();
        r.status = GateStatus::Clear;
        assert!(!pending_head_lock_only(&r));
        let mut r = pending();
        r.store_compliance.status = StoreStatus::Blocked;
        assert!(!pending_head_lock_only(&r));
    }
}
