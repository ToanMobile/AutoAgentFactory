//! Binary adversarial cross-audit (docs/PLAN.md C.9): a second agent run, in
//! a fresh context, answers a fixed yes/no checklist about a diff. Only a
//! clean answer becomes a signed approval for the head-lock fingerprint.

pub mod checklist;
pub mod diff_hunks;
pub mod prompt;
pub mod scoring;
pub mod verdict;

use std::path::Path;

use aaf_domain::config::RepoConfig;
use aaf_domain::gate::{Approval, GoldenScore};
use aaf_domain::ports::{AgentRequest, AgentRunner, Clock, PortError};
use aaf_domain::task::ImmutableSpec;

use crate::gate::{GateError, head_lock};

pub use checklist::{Item, build_checklist};
pub use diff_hunks::{
    Citation, DiffHunks, PathSpans, Span, evidence_in_diff, extract_citations,
    has_file_line_reference, parse_diff_hunks, parse_diff_hunks_with_truncation,
};
pub use prompt::{build_prompt, build_scored_prompt};
pub use scoring::{
    GOLDEN_IDS, GOLDEN_STANDARDS, MechanicalScoreReport, ScoreGateEvaluation,
    compute_mechanical_score, evaluate_score_gate,
};
pub use verdict::{
    AuditVerdict, Finding, ScoredParse, parse_scored_verdict, parse_verdict,
    validate_verdict_against_diff,
};

/// `runs.executor` of an audit run: it tells the auditor's run from the
/// author's, both in the audit itself and in the account history.
pub const AUDIT_EXECUTOR: &str = "cross-audit";

/// What the auditor judges. `spec` and `diff` are untrusted: the agent under
/// review wrote them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditInput {
    pub spec: ImmutableSpec,
    pub diff: String,
    /// Adds the advisory store-policy items.
    pub mobile: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditReport {
    pub verdict: AuditVerdict,
    pub checklist: Vec<Item>,
    /// Validated 7 Golden Standards scores (`G1..G7`) when present/required.
    pub scores: Vec<GoldenScore>,
    /// True when the auditor saw only the head of the diff.
    pub diff_truncated: bool,
    /// Usage of the auditor run, for the root budget.
    pub tokens_in: u64,
    pub tokens_out: u64,
}

fn inconclusive(reason: &str) -> AuditVerdict {
    AuditVerdict::Inconclusive {
        reason: reason.to_string(),
    }
}

/// Runs the auditor once and parses its answer. `request_template` supplies
/// run id, cwd, account, timeout and sandbox; its prompt is replaced.
///
/// A runner that could not complete the run (timeout, limit hit, non-zero
/// exit) yields `Inconclusive`; `Err` is only the runner's own port error.
pub fn run_audit(
    input: &AuditInput,
    runner: &dyn AgentRunner,
    request_template: &AgentRequest,
) -> Result<AuditReport, PortError> {
    run_scored_audit(input, runner, request_template, false)
}

/// Runs the auditor once and parses both the binary checklist and (when
/// `require_scores` is `true`) the 7 Golden Standards (`G1..G7`).
///
/// A runner that could not complete the run (timeout, limit hit, non-zero
/// exit) yields `Inconclusive`; `Err` is only the runner's own port error.
pub fn run_scored_audit(
    input: &AuditInput,
    runner: &dyn AgentRunner,
    request_template: &AgentRequest,
    require_scores: bool,
) -> Result<AuditReport, PortError> {
    let checklist = build_checklist(&input.spec, input.mobile);
    let (visible_diff, diff_truncated) = prompt::truncate_diff(&input.diff);
    let mut report = AuditReport {
        verdict: inconclusive("empty diff: nothing to audit"),
        checklist,
        scores: Vec::new(),
        diff_truncated,
        tokens_in: 0,
        tokens_out: 0,
    };
    // An approval for "no changes" would be an approval of nothing seen.
    if input.diff.trim().is_empty() {
        return Ok(report);
    }

    let mut request = request_template.clone();
    request.prompt = build_scored_prompt(input, require_scores);
    let outcome = runner.run(&request)?;
    report.tokens_in = outcome.tokens_in;
    report.tokens_out = outcome.tokens_out;

    if !outcome.succeeded() {
        report.verdict =
            inconclusive("auditor run did not complete (timeout, limit hit or non-zero exit)");
    } else {
        let parsed = parse_scored_verdict(&outcome.final_text, &report.checklist, require_scores);
        let hunks = parse_diff_hunks_with_truncation(visible_diff, diff_truncated);
        let parsed = validate_verdict_against_diff(parsed, &hunks);
        match parsed.verdict {
            // The auditor never saw the tail, so its "no" answers do not cover it.
            AuditVerdict::Pass { .. } if diff_truncated => {
                report.verdict =
                    inconclusive("diff was truncated: the unseen part cannot be approved");
            }
            verdict => {
                report.verdict = verdict;
                report.scores = parsed.scores;
            }
        }
    }
    Ok(report)
}

/// Writes a signed `PASS` approval for `lock_fp` when, and only when, the
/// report passed. Returns `Ok(None)` and writes nothing otherwise.
/// `same_account` records that the auditor shared the author's account.
pub fn approve_if_pass(
    report: &AuditReport,
    data_dir: &Path,
    repo_root: &Path,
    lock_fp: &str,
    reviewer: &str,
    same_account: bool,
    clock: &dyn Clock,
) -> Result<Option<Approval>, PortError> {
    // `diff_truncated` is re-checked because report fields are public.
    if !report.verdict.is_pass() || report.diff_truncated {
        return Ok(None);
    }
    if lock_fp.trim().is_empty() || reviewer.trim().is_empty() {
        return Err(PortError::Invalid(
            "approval needs a lock fingerprint and a reviewer".to_string(),
        ));
    }
    let approval = Approval {
        lock_fp: lock_fp.to_string(),
        reviewer: reviewer.to_string(),
        verdict: "PASS".to_string(),
        same_account,
        created_at: clock.now(),
    };
    let io = |err: aaf_fp::FpError| PortError::Io(err.to_string());
    let key = aaf_fp::load_or_create_key(data_dir).map_err(io)?;
    head_lock::write_approval_everywhere(data_dir, &key, repo_root, &approval)
        .map_err(|err| PortError::Io(err.to_string()))?;
    Ok(Some(approval))
}

/// Writes signed `PASS` approvals (with TOCTOU `lock_fp` verification, derived
/// base/tracked views, and mirroring to the common repository root) via
/// [`head_lock::approve_matching`] when, and only when, the report passed.
#[allow(clippy::too_many_arguments)]
pub fn approve_if_pass_with_config(
    report: &AuditReport,
    data_dir: &Path,
    repo_root: &Path,
    cfg: &RepoConfig,
    lock_fp: &str,
    reviewer: &str,
    same_account: bool,
    clock: &dyn Clock,
) -> Result<Option<Approval>, PortError> {
    if !report.verdict.is_pass() || report.diff_truncated {
        return Ok(None);
    }
    if lock_fp.trim().is_empty() || reviewer.trim().is_empty() {
        return Err(PortError::Invalid(
            "approval needs a lock fingerprint and a reviewer".to_string(),
        ));
    }
    let approval = head_lock::approve_matching(
        data_dir,
        repo_root,
        cfg,
        reviewer,
        same_account,
        lock_fp,
        clock,
    )
    .map_err(|err| match err {
        GateError::NothingLocked | GateError::LockFpChanged | GateError::Config(_) => {
            PortError::Invalid(err.to_string())
        }
        other => PortError::Io(other.to_string()),
    })?;
    Ok(Some(approval))
}
