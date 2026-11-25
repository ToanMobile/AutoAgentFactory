//! v3.2 handover report builder and strictly read-only loader.

use aaf_domain::config::ProofConfig;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::gate::{GateStatus, GoldenScore, PostGateAuditRecord, Receipt};
use aaf_domain::pipeline::ReviewerVerdictRecord;
use aaf_domain::proof::ProofRecord;
use aaf_domain::task::{Task, TaskState};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cross_audit::scoring::GOLDEN_STANDARDS;

mod load;
mod sections;

pub use crate::handover_export::{AutoHandoverPaths, HandoverError, write_auto_handover_reports};
pub use crate::handover_html::{
    CSP_META, HandoverProof, MAX_HTML_PROOF_BYTES, MAX_PR_BODY_BYTES, compose_pr_body,
    encode_base64, escape_html, render_html, render_json, render_markdown,
    sanitize_external_markdown,
};
pub use load::load_handover;

use sections::{
    BuildCtx, ScoreBundle, build_code_safety, build_gate_proof_next_line, build_new_bug_risk,
    build_next_step, build_prevent_old_bug, build_unverified, build_user_outcome,
    extract_root_cause, map_gate_commands, resolve_scores, verdict_label_for,
};

/// Solo-dev badge surfaced whenever audit ran on the same account without a
/// strict `require_different_provider` failure.
pub const SOLO_DEV_BADGE: &str = "same_account — chưa có auditor độc lập";

const MAX_EVENTS_LOAD: u32 = 200;

/// Single gate command execution entry surfaced in handover QA tables.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoverGateCommand {
    pub name: String,
    pub command: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u64,
}

/// Structured acceptance handover report for a single task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoverData {
    pub task_id: String,
    pub title: String,
    pub state: String,
    pub repo_root: String,
    pub tree_fp: Option<String>,
    /// `"XONG"` only when all required evidence is present and passing; otherwise `"CHƯA XONG"`.
    pub status: String,
    #[serde(default)]
    pub verdict_label: String,
    #[serde(default)]
    pub generated_at: i64,
    #[serde(default)]
    pub root_cause: String,
    #[serde(default)]
    pub user_outcome: String,
    #[serde(default)]
    pub gate_proof_next_line: String,
    #[serde(default)]
    pub next_step: String,
    #[serde(default)]
    pub gate_commands: Vec<HandoverGateCommand>,
    pub missing: Vec<String>,
    pub what_done: String,
    pub prevent_old_bug: String,
    pub new_bug_risk: String,
    pub code_safety: String,
    pub unverified: String,
    pub mechanical_score: Option<u8>,
    pub llm_average: Option<u8>,
    pub effective_score: Option<u8>,
    pub scores: Vec<GoldenScore>,
    #[serde(default)]
    pub dual_consensus: bool,
    #[serde(default)]
    pub adjudicated: bool,
    #[serde(default)]
    pub reviewers: Vec<ReviewerVerdictRecord>,
    pub same_account_warning: bool,
    pub proof_required: bool,
    pub proof: Option<HandoverProof>,
    #[serde(skip)]
    pub proof_config: ProofConfig,
}

#[derive(Default, Deserialize)]
#[serde(default)]
pub(super) struct CompactGate {
    pub(super) status: Option<String>,
    pub(super) tree_fp: Option<String>,
    pub(super) gated_tree: Option<String>,
    pub(super) config_hash: Option<String>,
    pub(super) reasons: Vec<String>,
    #[serde(alias = "commands_ok")]
    pub(super) commands_passed: Option<u64>,
    #[serde(alias = "commands_run")]
    pub(super) commands_total: Option<u64>,
    pub(super) static_critical: Option<u64>,
    #[serde(alias = "static_warnings")]
    pub(super) static_warn: Option<u64>,
    pub(super) suppressions: Option<u64>,
    pub(super) store_status: Option<String>,
    pub(super) store_critical: Option<u64>,
    pub(super) store_warnings: Option<u64>,
    pub(super) store_unresolved: Value,
    pub(super) head_lock_status: Option<String>,
    pub(super) mechanical_score: Option<u8>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
pub(super) struct CompactAudit {
    pub(super) verdict: Option<String>,
    pub(super) reviewer: Option<String>,
    pub(super) same_account: Option<bool>,
    pub(super) mechanical_score: Option<u8>,
    pub(super) llm_average: Option<u8>,
    pub(super) effective_score: Option<u8>,
    pub(super) scores: Vec<GoldenScore>,
    pub(super) tree_fp: Option<String>,
    pub(super) config_hash: Option<String>,
    #[serde(default)]
    pub(super) dual_consensus: bool,
    #[serde(default)]
    pub(super) adjudicated: bool,
    #[serde(default)]
    pub(super) reviewers: Vec<ReviewerVerdictRecord>,
}

impl CompactAudit {
    fn to_dual_record(&self, task: &Task, bundle: &ScoreBundle) -> Option<PostGateAuditRecord> {
        (self.dual_consensus || !self.reviewers.is_empty()).then(|| PostGateAuditRecord {
            repo_root: task.repo_root.clone(),
            tree_fp: self.tree_fp.clone().unwrap_or_default(),
            config_hash: self.config_hash.clone().unwrap_or_default(),
            verdict: self.verdict.clone().unwrap_or_default(),
            reviewer: self.reviewer.clone().unwrap_or_default(),
            same_account: self.same_account.unwrap_or(false),
            mechanical_score: bundle.mechanical.unwrap_or(0),
            llm_average: bundle.llm_average,
            effective_score: bundle.effective.unwrap_or(0),
            scores: self.scores.clone(),
            dual_consensus: self.dual_consensus,
            adjudicated: self.adjudicated,
            reviewers: self.reviewers.clone(),
            created_at: 0,
        })
    }
}

pub(super) fn latest_event(events: &[FactoryEvent], kind: EventKind) -> Option<&FactoryEvent> {
    events
        .iter()
        .filter(|e| e.kind == kind)
        .max_by_key(|e| (e.created_at, e.id.unwrap_or(0)))
}

pub(super) fn parse_event<T: for<'de> Deserialize<'de>>(
    events: &[FactoryEvent],
    kind: EventKind,
) -> Option<T> {
    serde_json::from_str(&latest_event(events, kind)?.payload_json).ok()
}

pub(super) fn non_empty(opt: &Option<String>) -> Option<&str> {
    opt.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

pub(super) fn enum_label<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

/// Assembles a [`HandoverData`] report from explicit task, event, receipt,
/// audit, and proof parts. Works even after a worktree has been cleaned by
/// falling back to compact `GateResult` and `CrossAudit` event payloads.
#[allow(clippy::too_many_arguments)]
pub fn build_handover_from_parts(
    task: &Task,
    events: &[FactoryEvent],
    receipt: Option<&Receipt>,
    audit: Option<&PostGateAuditRecord>,
    proof: Option<&ProofRecord>,
    proof_required: bool,
    score_gate: bool,
    min_average: u8,
    min_criterion: u8,
    require_different_provider: bool,
) -> HandoverData {
    build_handover_from_parts_with_dual(
        task,
        events,
        receipt,
        audit,
        proof,
        proof_required,
        score_gate,
        min_average,
        min_criterion,
        require_different_provider,
        false,
    )
}

/// Assembles a [`HandoverData`] report with explicit Dual-Review policy enforcement.
#[allow(clippy::too_many_arguments)]
pub fn build_handover_from_parts_with_dual(
    task: &Task,
    events: &[FactoryEvent],
    receipt: Option<&Receipt>,
    audit: Option<&PostGateAuditRecord>,
    proof: Option<&ProofRecord>,
    proof_required: bool,
    score_gate: bool,
    min_average: u8,
    min_criterion: u8,
    require_different_provider: bool,
    dual_review: bool,
) -> HandoverData {
    let cg: Option<CompactGate> = parse_event(events, EventKind::GateResult);
    let ca: Option<CompactAudit> = parse_event(events, EventKind::CrossAudit);
    let same_account = audit
        .map(|a| a.same_account)
        .or_else(|| {
            receipt
                .and_then(|r| r.head_lock.approval.as_ref())
                .map(|a| a.same_account)
        })
        .or_else(|| ca.as_ref().and_then(|a| a.same_account))
        .unwrap_or(false);
    let bundle = resolve_scores(
        receipt,
        audit,
        cg.as_ref(),
        ca.as_ref(),
        !proof_required || proof.is_some(),
        require_different_provider,
        same_account,
    );
    let ctx = BuildCtx {
        task,
        receipt,
        cg: cg.as_ref(),
        audit,
        ca: ca.as_ref(),
        proof,
        proof_required,
        audit_verdict: audit
            .map(|a| a.verdict.as_str())
            .or_else(|| ca.as_ref().and_then(|a| non_empty(&a.verdict))),
        audit_reviewer: audit
            .map(|a| a.reviewer.as_str())
            .or_else(|| ca.as_ref().and_then(|a| non_empty(&a.reviewer)))
            .or_else(|| {
                receipt
                    .and_then(|r| r.head_lock.approval.as_ref())
                    .map(|a| a.reviewer.as_str())
            }),
        bundle,
    };
    let missing = collect_missing(
        &ctx,
        dual_review,
        require_different_provider,
        score_gate,
        min_average,
        min_criterion,
    );
    let status = if missing.is_empty() {
        "XONG"
    } else {
        "CHƯA XONG"
    }
    .to_string();
    let tree_fp = receipt
        .map(|r| r.tree_fp.clone())
        .or_else(|| audit.map(|a| a.tree_fp.clone()))
        .or_else(|| proof.map(|p| p.tree_fp.clone()))
        .or_else(|| {
            cg.as_ref()
                .and_then(|g| non_empty(&g.tree_fp))
                .map(str::to_string)
        })
        .or_else(|| {
            ca.as_ref()
                .and_then(|a| non_empty(&a.tree_fp))
                .map(str::to_string)
        });
    let (dual_consensus, adjudicated, reviewers) = match (audit, ca.as_ref()) {
        (Some(a), _) => (a.dual_consensus, a.adjudicated, a.reviewers.clone()),
        (None, Some(a)) => (a.dual_consensus, a.adjudicated, a.reviewers.clone()),
        (None, None) => (false, false, Vec::new()),
    };

    let what_done = crate::handover_dual::build_clean_what_done(task, events).join(" · ");
    let handover_proof = proof.map(HandoverProof::from_record);
    let verdict_label = verdict_label_for(&status);
    let generated_at = receipt.map(|r| r.created_at).unwrap_or(task.updated_at);
    let root_cause = extract_root_cause(task, events, &what_done);
    let user_outcome = build_user_outcome(task, &what_done);
    let next_step = build_next_step(task, &status, &missing);
    let gate_proof_next_line = build_gate_proof_next_line(
        &ctx,
        tree_fp.as_deref(),
        handover_proof.as_ref(),
        &next_step,
    );
    let gate_commands = map_gate_commands(receipt);

    HandoverData {
        task_id: task.id.clone(),
        title: task.title.clone(),
        state: task.state.as_str().to_string(),
        repo_root: task.repo_root.clone(),
        tree_fp,
        status,
        verdict_label,
        generated_at,
        root_cause,
        user_outcome,
        gate_proof_next_line,
        next_step,
        gate_commands,
        what_done,
        prevent_old_bug: build_prevent_old_bug(&ctx),
        new_bug_risk: build_new_bug_risk(&ctx),
        code_safety: build_code_safety(&ctx),
        unverified: build_unverified(&ctx, &missing),
        missing,
        mechanical_score: ctx.bundle.mechanical,
        llm_average: ctx.bundle.llm_average,
        effective_score: ctx.bundle.effective,
        scores: ctx.bundle.scores,
        dual_consensus,
        adjudicated,
        reviewers,
        same_account_warning: ctx.bundle.same_account_warning,
        proof_required,
        proof: handover_proof,
        proof_config: ProofConfig::default(),
    }
}

fn collect_missing(
    ctx: &BuildCtx<'_>,
    dual_review: bool,
    require_different_provider: bool,
    score_gate: bool,
    min_avg: u8,
    min_crit: u8,
) -> Vec<String> {
    let mut missing = Vec::new();
    if !matches!(ctx.task.state, TaskState::Done | TaskState::Review) {
        missing.push(format!(
            "task state is {} (expected review or done)",
            ctx.task.state.as_str()
        ));
    }
    match ctx.receipt {
        Some(r) if r.status == GateStatus::Clear => {}
        Some(r) => missing.push(format!(
            "gate receipt status is {} (expected CLEAR)",
            enum_label(&r.status)
        )),
        None if ctx
            .cg
            .and_then(|g| non_empty(&g.status))
            .is_some_and(|s| s.eq_ignore_ascii_case("clear")) => {}
        None => missing.push("missing verified CLEAR gate receipt or GateResult event".into()),
    }
    if ctx.proof_required && ctx.proof.is_none() {
        missing.push("missing verified device proof (proof.required = true)".into());
    }
    if let Some(v) = ctx.audit_verdict {
        let lower = v.trim().to_ascii_lowercase();
        if lower == "fail" || lower.starts_with("fail:") {
            missing.push(format!("cross-audit verdict is {v}"));
        }
    }
    if score_gate {
        if !ctx
            .audit_verdict
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("pass"))
        {
            missing
                .push("missing passing post-gate cross-audit verdict (score_gate = true)".into());
        }
        match ctx.bundle.effective {
            Some(eff) if eff >= min_avg => {}
            Some(eff) => missing.push(format!(
                "effective_score {eff}/100 is below min_average {min_avg}"
            )),
            None => missing.push("missing effective_score (score_gate = true)".into()),
        }
        for (id, _) in GOLDEN_STANDARDS {
            match ctx.bundle.scores.iter().find(|s| s.id == id) {
                Some(s) if s.score >= min_crit => {}
                Some(s) => missing.push(format!(
                    "criterion {id} scored {}/100 (min {min_crit})",
                    s.score
                )),
                None => missing.push(format!("missing Golden Standard score for {id}")),
            }
        }
    }
    let compact_audit = ctx
        .audit
        .is_none()
        .then(|| ctx.ca.and_then(|a| a.to_dual_record(ctx.task, &ctx.bundle)))
        .flatten();
    missing.extend(crate::handover_dual::check_dual_review_missing(
        ctx.audit.or(compact_audit.as_ref()),
        dual_review,
        require_different_provider,
        score_gate,
        min_avg,
        min_crit,
    ));
    missing
}
