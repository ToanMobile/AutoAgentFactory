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

pub use matrix::{
    DeferredSiblingOutcome, DualMatrixDecision, build_dual_audit_record, decide_dual_matrix,
    dual_audit_satisfies_policy, governing_reviewer, merge_deferred_outcome, validate_dual_quorum,
};
use matrix::{is_inconclusive_partial, issues_to_findings};
pub(crate) use turn::{run_adjudicate_turn, run_reviewer_turn};

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
    let proof_ok = post_audit::proof_verified(deps, &job.worktree.path, receipt);
    compute_mechanical_score(receipt, proof_ok, false, false).score
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
) -> PortResult<Next> {
    let note = format!("post-gate cross-audit rejected the changes ({reason})");
    deps.store
        .append_context_note(&task_id, &unblock::one_line(&note, 400), deps.now())?;
    let signal = audit_rejection_signal("post-gate cross-audit rejected the changes");
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
    reject_dual_audit(deps, task.id.clone(), job.worktree.clone(), summary)
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
            let next = reject_dual_audit(deps, task.id, job.worktree.clone(), &verdict)?;
            Ok(DualFinalizeResult::Done(next))
        }
    }
}
