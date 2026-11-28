//! Blind Parallel Post-Code Dual-Review coordination, quorum validation,
//! governing-reviewer projection, 6-branch deterministic matrix, and turn
//! execution helpers (`docs/PLAN_DUAL_FRONTIER.md` §D.4).

use aaf_domain::event::EventKind;
use aaf_domain::gate::{PostGateAuditRecord, Receipt};
use aaf_domain::pipeline::{ReviewIssue, ReviewerConfig};
use aaf_domain::ports::{PortError, PortResult};
use aaf_domain::task::Task;
use aaf_fp::{PartialAuditKey, PartialAuditRecord};
use serde_json::json;
mod matrix;
mod turn;

pub(crate) use matrix::is_inconclusive_partial;
use matrix::issues_to_findings;
pub use matrix::{
    DeferredSiblingOutcome, DualMatrixDecision, build_dual_audit_record, decide_dual_matrix,
    dual_audit_satisfies_policy, governing_reviewer, merge_deferred_outcome, validate_dual_quorum,
};
#[allow(unused_imports)]
pub(crate) use turn::{ReviewInputs, ReviewPolicy, run_adjudicate_turn, run_reviewer_turn};

use super::audit_backend::audit_rejection_signal;
use super::post_audit;
use super::worker::{GateJob, Next};
use super::{Deps, unblock};
use crate::cross_audit::compute_mechanical_score;
use crate::git::TaskWorktree;

/// Coordinator state tracking blind parallel sibling turns for one `Gating` task (R3-CONC-1).
pub(crate) struct DualReviewCoord {
    pub in_flight: usize,
    pub aborted: bool,
    pub deferred_outcome: Option<DeferredSiblingOutcome>,
    pub inconclusive_partials: Vec<PartialAuditRecord>,
    pub job: GateJob,
    pub receipt: Box<Receipt>,
    pub plan_hash: String,
}

/// Result of executing one blind reviewer turn.
pub(crate) enum PartialTurnResult {
    Completed(Box<PartialAuditRecord>),
    Deferred(Next),
}

/// Result of aggregating completed dual-review partials on the scheduler.
pub(crate) enum DualFinalizeResult {
    Done(Next),
    NeedsAdjudication {
        pass_reviewer_id: String,
        findings: Vec<ReviewIssue>,
        partials: Vec<PartialAuditRecord>,
    },
}

fn fp_io(err: aaf_fp::FpError) -> PortError {
    PortError::Io(err.to_string())
}

fn canon_root(job: &GateJob) -> String {
    std::fs::canonicalize(&job.worktree.path)
        .unwrap_or_else(|_| job.worktree.path.clone())
        .to_string_lossy()
        .into_owned()
}

fn dual_mech_score(deps: &Deps, job: &GateJob, receipt: &Receipt) -> u8 {
    dual_mech_score_with(deps, job, receipt, None)
}

/// [`dual_mech_score`] with the proof verdict optionally given by the caller.
///
/// `Some(ok)` is `aaf board adopt`: the receipt comes from a dry run on the
/// merged tree whose scratch worktree is gone, so the proof cannot be asked
/// again, and a receipt that is `Blocked` only by the head lock (which adopt
/// settles itself, after both reviewers) is scored as if it were `Clear`. Any
/// other `Blocked` receipt keeps its score of 0.
pub(crate) fn dual_mech_score_with(
    deps: &Deps,
    job: &GateJob,
    receipt: &Receipt,
    proof_ok: Option<bool>,
) -> u8 {
    let Some(proof_ok) = proof_ok else {
        let proof_ok = post_audit::proof_verified(deps, &job.worktree.path, receipt);
        return compute_mechanical_score(receipt, proof_ok, false, false).score;
    };
    let mut scored = receipt.clone();
    if crate::pr_ops::pr_verify::pending_head_lock_only(&scored) {
        scored.status = aaf_domain::gate::GateStatus::Clear;
    }
    compute_mechanical_score(&scored, proof_ok, false, false).score
}

/// Loads valid signed `audit_partial\0` records bound to `(repo_root, tree_fp,
/// base_sha, config_hash, plan_contract_hash, reviewer_id, executor, model, effort)`.
pub(crate) fn missing_reviewers(
    deps: &Deps,
    job: &GateJob,
    receipt: &Receipt,
    plan_hash: &str,
) -> PortResult<(Vec<ReviewerConfig>, Vec<PartialAuditRecord>)> {
    let base_sha = receipt.base_sha.as_deref().unwrap_or("");
    let key = aaf_fp::load_key(&deps.settings.data_dir).ok();
    let (mut missing, mut cached) = (Vec::new(), Vec::new());
    for rev in &deps.settings.repo.cross_audit.reviewers {
        let binding = PartialAuditKey {
            repo_root: &job.worktree.path,
            tree_fp: &receipt.tree_fp,
            base_sha,
            config_hash: &receipt.config_hash,
            plan_contract_hash: plan_hash,
            reviewer_id: &rev.id,
            executor: &rev.executor,
            model: &rev.model,
            effort: &rev.effort,
        };
        let valid = key
            .as_deref()
            .and_then(|k| aaf_fp::read_partial_audit(&deps.settings.data_dir, k, &binding).ok())
            .flatten()
            .filter(|rec| !is_inconclusive_partial(rec));
        match valid {
            Some(rec) => cached.push(rec),
            None => missing.push(rev.clone()),
        }
    }
    Ok((missing, cached))
}

fn reject_dual_audit(
    deps: &Deps,
    task_id: String,
    worktree: TaskWorktree,
    reason: &str,
    findings: &[crate::cross_audit::Finding],
) -> PortResult<Next> {
    let now = deps.now();
    let note = format!("post-gate cross-audit rejected the changes ({reason})");
    deps.store
        .append_context_note(&task_id, &unblock::one_line(&note, 400), now)?;
    if let Some(safe) = unblock::format_sanitized_audit_findings(findings) {
        let f_note = format!(
            "post_audit_findings: <untrusted-data id=\"audit-findings\">{safe}</untrusted-data>"
        );
        deps.store.append_context_note(&task_id, &f_note, now)?;
    }
    let signal_tail = if reason.contains("diff was truncated") {
        format!("audit_diff_truncated: {reason}")
    } else {
        "post-gate cross-audit rejected the changes".to_string()
    };
    let signal = audit_rejection_signal(&signal_tail);
    Ok(Next::Unblock {
        task_id,
        worktree,
        signal: Box::new(signal),
    })
}

fn sign_and_emit_pass(
    deps: &Deps,
    task_id: &str,
    job: &GateJob,
    receipt: &Receipt,
    rec: &PostGateAuditRecord,
) -> PortResult<Next> {
    let key = aaf_fp::load_or_create_key(&deps.settings.data_dir).map_err(fp_io)?;
    aaf_fp::write_audit_verdict(&deps.settings.data_dir, &key, &job.worktree.path, rec)
        .map_err(fp_io)?;
    deps.emit(
        EventKind::CrossAudit,
        task_id,
        json!({
            "phase": "post_gate", "verdict": "pass", "dual_consensus": true,
            "adjudicated": rec.adjudicated, "reviewer": rec.reviewer, "reviewers": rec.reviewers,
            "same_account": false, "mechanical_score": rec.mechanical_score,
            "llm_average": rec.llm_average, "effective_score": rec.effective_score,
            "scores": rec.scores, "tree_fp": receipt.tree_fp, "config_hash": receipt.config_hash,
        }),
    )?;
    Ok(Next::Gate(GateJob {
        audited: true,
        cached_receipt: Some(Box::new(receipt.clone())),
        ..job.clone()
    }))
}

#[allow(clippy::too_many_arguments)]
fn emit_fail_and_rework(
    deps: &Deps,
    task: &Task,
    job: &GateJob,
    receipt: &Receipt,
    mech: u8,
    adjudicated: bool,
    findings: &[ReviewIssue],
    summary: &str,
) -> PortResult<Next> {
    deps.emit(
        EventKind::CrossAudit,
        &task.id,
        json!({
            "phase": "post_gate", "verdict": format!("fail: {summary}"),
            "dual_consensus": false, "adjudicated": adjudicated, "mechanical_score": mech,
            "tree_fp": receipt.tree_fp, "config_hash": receipt.config_hash,
        }),
    )?;
    let converted = issues_to_findings(findings);
    if let Some(next) = unblock::handle_audit_failure(deps, task, &converted)? {
        return Ok(next);
    }
    reject_dual_audit(
        deps,
        task.id.clone(),
        job.worktree.clone(),
        summary,
        &converted,
    )
}

fn partial_verdict_body(verdict: &str) -> &str {
    let rest = verdict
        .strip_prefix("INCONCLUSIVE:")
        .unwrap_or(verdict)
        .trim_start();
    rest.strip_prefix("inconclusive:")
        .unwrap_or(rest)
        .trim_start()
}

fn partials_have_prefix(partials: &[PartialAuditRecord], prefix: &str) -> bool {
    partials
        .iter()
        .any(|p| is_inconclusive_partial(p) && partial_verdict_body(&p.verdict).starts_with(prefix))
}

fn is_deterministic_quorum_inconclusive(reason: &str) -> bool {
    reason.starts_with("dual review requires >= 2 reviewers")
        || reason.starts_with("dual review consensus requires >= 2 distinct")
}

/// Aggregates all completed partials via [`decide_dual_matrix`], emitting 1 `CrossAudit` per round.
pub(crate) fn finalize_dual_review(
    deps: &Deps,
    job: &GateJob,
    receipt: &Receipt,
    partials: &[PartialAuditRecord],
) -> PortResult<DualFinalizeResult> {
    let task = deps.store.get_task(&job.task_id)?;
    let cfg = &deps.settings.repo.cross_audit;
    let mech = dual_mech_score(deps, job, receipt);
    match decide_dual_matrix(
        partials,
        cfg,
        &canon_root(job),
        &receipt.tree_fp,
        &receipt.config_hash,
        mech,
        deps.now(),
    ) {
        DualMatrixDecision::ConsensusPass { record } => {
            let next = sign_and_emit_pass(deps, &task.id, job, receipt, &record)?;
            Ok(DualFinalizeResult::Done(next))
        }
        DualMatrixDecision::NeedsAdjudication {
            pass_reviewer_id,
            findings,
            partials,
        } => Ok(DualFinalizeResult::NeedsAdjudication {
            pass_reviewer_id,
            findings,
            partials,
        }),
        DualMatrixDecision::Rework { findings, summary } => {
            let next =
                emit_fail_and_rework(deps, &task, job, receipt, mech, false, &findings, &summary)?;
            Ok(DualFinalizeResult::Done(next))
        }
        DualMatrixDecision::Inconclusive { reason } => {
            let verdict = format!("inconclusive: {reason}");
            deps.emit(
                EventKind::CrossAudit,
                &task.id,
                json!({
                    "phase": "post_gate", "verdict": verdict, "dual_consensus": false,
                    "adjudicated": false, "mechanical_score": mech,
                    "tree_fp": receipt.tree_fp, "config_hash": receipt.config_hash,
                }),
            )?;
            if is_deterministic_quorum_inconclusive(&reason) {
                unblock::block_untried(deps, &task.id, &verdict)?;
                return Ok(DualFinalizeResult::Done(Next::Blocked));
            }
            let diff_truncated = partials_have_prefix(partials, "diff was truncated");
            let empty_diff = partials_have_prefix(partials, "empty diff");
            if !diff_truncated
                && !empty_diff
                && super::infra_retry::block_inconclusive_audit(deps, &task, true, false, &verdict)?
            {
                return Ok(DualFinalizeResult::Done(Next::Blocked));
            }
            let next = reject_dual_audit(deps, task.id, job.worktree.clone(), &verdict, &[])?;
            Ok(DualFinalizeResult::Done(next))
        }
    }
}
