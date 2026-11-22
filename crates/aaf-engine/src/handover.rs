//! v3.2 handover report builder and strictly read-only loader.

use aaf_domain::config::ProofConfig;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::gate::{GateStatus, GoldenScore, PostGateAuditRecord, Receipt, Severity};
use aaf_domain::proof::ProofRecord;
use aaf_domain::task::{Task, TaskState};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cross_audit::scoring::{GOLDEN_STANDARDS, compute_mechanical_score};

pub use crate::handover_html::{
    CSP_META, HandoverProof, MAX_HTML_PROOF_BYTES, MAX_PR_BODY_BYTES, compose_pr_body,
    encode_base64, escape_html, render_html, render_json, render_markdown,
};

/// Solo-dev badge surfaced whenever audit ran on the same account without a
/// strict `require_different_provider` failure.
pub const SOLO_DEV_BADGE: &str = "same_account — chưa có auditor độc lập";

const MAX_EVENTS_LOAD: u32 = 200;

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
    pub same_account_warning: bool,
    pub proof_required: bool,
    pub proof: Option<HandoverProof>,
    #[serde(skip)]
    pub proof_config: ProofConfig,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct CompactGate {
    status: Option<String>,
    tree_fp: Option<String>,
    gated_tree: Option<String>,
    config_hash: Option<String>,
    reasons: Vec<String>,
    #[serde(alias = "commands_ok")]
    commands_passed: Option<u64>,
    #[serde(alias = "commands_run")]
    commands_total: Option<u64>,
    static_critical: Option<u64>,
    #[serde(alias = "static_warnings")]
    static_warn: Option<u64>,
    suppressions: Option<u64>,
    store_status: Option<String>,
    store_critical: Option<u64>,
    store_warnings: Option<u64>,
    store_unresolved: Value,
    head_lock_status: Option<String>,
    mechanical_score: Option<u8>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct CompactAudit {
    verdict: Option<String>,
    reviewer: Option<String>,
    same_account: Option<bool>,
    mechanical_score: Option<u8>,
    llm_average: Option<u8>,
    effective_score: Option<u8>,
    scores: Vec<GoldenScore>,
    tree_fp: Option<String>,
    config_hash: Option<String>,
}

struct ScoreBundle {
    mechanical: Option<u8>,
    llm_average: Option<u8>,
    effective: Option<u8>,
    scores: Vec<GoldenScore>,
    same_account_warning: bool,
}

struct BuildCtx<'a> {
    task: &'a Task,
    events: &'a [FactoryEvent],
    receipt: Option<&'a Receipt>,
    cg: Option<&'a CompactGate>,
    proof: Option<&'a ProofRecord>,
    proof_required: bool,
    audit_verdict: Option<&'a str>,
    audit_reviewer: Option<&'a str>,
    bundle: ScoreBundle,
}

fn latest_event(events: &[FactoryEvent], kind: EventKind) -> Option<&FactoryEvent> {
    events
        .iter()
        .filter(|e| e.kind == kind)
        .max_by_key(|e| (e.created_at, e.id.unwrap_or(0)))
}

fn parse_event<T: for<'de> Deserialize<'de>>(
    events: &[FactoryEvent],
    kind: EventKind,
) -> Option<T> {
    serde_json::from_str(&latest_event(events, kind)?.payload_json).ok()
}

fn non_empty(opt: &Option<String>) -> Option<&str> {
    opt.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

fn enum_label<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

fn one_line(s: &str, max_chars: usize) -> String {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = collapsed.chars().take(max_chars).collect();
    if collapsed.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

fn resolve_scores(
    receipt: Option<&Receipt>,
    audit: Option<&PostGateAuditRecord>,
    cg: Option<&CompactGate>,
    ca: Option<&CompactAudit>,
    proof_ok: bool,
    require_diff: bool,
    same_account: bool,
) -> ScoreBundle {
    let solo_warn = same_account && !require_diff;
    if let Some(a) = audit {
        return ScoreBundle {
            mechanical: Some(a.mechanical_score),
            llm_average: a.llm_average,
            effective: Some(a.effective_score),
            scores: a.scores.clone(),
            same_account_warning: a.same_account && !require_diff,
        };
    }
    if let Some(ca) = ca.filter(|a| a.effective_score.is_some() || a.mechanical_score.is_some()) {
        let mech = ca.mechanical_score.or_else(|| {
            receipt.map(|r| compute_mechanical_score(r, proof_ok, require_diff, same_account).score)
        });
        let eff = ca.effective_score.or_else(|| match (mech, ca.llm_average) {
            (Some(m), Some(l)) => Some(m.min(l)),
            (m, l) => m.or(l),
        });
        return ScoreBundle {
            mechanical: mech,
            llm_average: ca.llm_average,
            effective: eff,
            scores: ca.scores.clone(),
            same_account_warning: solo_warn,
        };
    }
    if let Some(r) = receipt {
        let mech = compute_mechanical_score(r, proof_ok, require_diff, same_account);
        return ScoreBundle {
            mechanical: Some(mech.score),
            llm_average: None,
            effective: Some(mech.score),
            scores: Vec::new(),
            same_account_warning: mech.same_account_warning || solo_warn,
        };
    }
    let mech = cg.and_then(|g| g.mechanical_score);
    ScoreBundle {
        mechanical: mech,
        llm_average: None,
        effective: mech,
        scores: Vec::new(),
        same_account_warning: solo_warn,
    }
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
        events,
        receipt,
        cg: cg.as_ref(),
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
    let missing = collect_missing(&ctx, score_gate, min_average, min_criterion);
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

    HandoverData {
        task_id: task.id.clone(),
        title: task.title.clone(),
        state: task.state.as_str().to_string(),
        repo_root: task.repo_root.clone(),
        tree_fp,
        status,
        what_done: build_what_done(&ctx),
        prevent_old_bug: build_prevent_old_bug(&ctx),
        new_bug_risk: build_new_bug_risk(&ctx),
        code_safety: build_code_safety(&ctx),
        unverified: build_unverified(&ctx, &missing),
        missing,
        mechanical_score: ctx.bundle.mechanical,
        llm_average: ctx.bundle.llm_average,
        effective_score: ctx.bundle.effective,
        scores: ctx.bundle.scores,
        same_account_warning: ctx.bundle.same_account_warning,
        proof_required,
        proof: proof.map(HandoverProof::from_record),
        proof_config: ProofConfig::default(),
    }
}

fn collect_missing(ctx: &BuildCtx<'_>, score_gate: bool, min_avg: u8, min_crit: u8) -> Vec<String> {
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
        None => missing.push("missing verified CLEAR gate receipt or GateResult event".to_string()),
    }
    if ctx.proof_required && ctx.proof.is_none() {
        missing.push("missing verified device proof (proof.required = true)".to_string());
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
            missing.push(
                "missing passing post-gate cross-audit verdict (score_gate = true)".to_string(),
            );
        }
        match ctx.bundle.effective {
            Some(eff) if eff >= min_avg => {}
            Some(eff) => missing.push(format!(
                "effective_score {eff}/100 is below min_average {min_avg}"
            )),
            None => missing.push("missing effective_score (score_gate = true)".to_string()),
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
    missing
}

fn build_what_done(ctx: &BuildCtx<'_>) -> String {
    let t = ctx.task;
    let mut parts = vec![format!(
        "{} (task {}, state: {})",
        one_line(&t.title, 160),
        t.id,
        t.state.as_str()
    )];
    let desc = one_line(&t.spec.description, 220);
    if !desc.is_empty() {
        parts.push(format!("Spec: {desc}"));
    }
    if !t.spec.acceptance.is_empty() {
        parts.push(format!("AC: {} item(s)", t.spec.acceptance.len()));
    }
    if let Some(note) = t
        .context_notes
        .last()
        .map(|n| one_line(n, 180))
        .filter(|n| !n.is_empty())
    {
        parts.push(format!("Notes: {note}"));
    } else if let Some(plan) = latest_event(ctx.events, EventKind::PlanGenerated)
        .and_then(|e| serde_json::from_str::<Value>(&e.payload_json).ok())
        .and_then(|v| {
            v.get("plan")
                .and_then(Value::as_str)
                .map(|s| one_line(s, 180))
        })
        .filter(|s| !s.is_empty())
    {
        parts.push(format!("Plan: {plan}"));
    }
    parts.join(" · ")
}

fn build_prevent_old_bug(ctx: &BuildCtx<'_>) -> String {
    if let Some(r) = ctx.receipt {
        if r.commands.is_empty() {
            return "no gate commands recorded [tự chạy]".to_string();
        }
        let passed = r.commands.iter().filter(|c| c.passed()).count();
        let summary: Vec<String> = r
            .commands
            .iter()
            .map(|c| {
                let st = if c.passed() {
                    "PASS"
                } else if c.timed_out {
                    "TIMEOUT"
                } else {
                    "FAIL"
                };
                format!("{}: {st} ({}ms)", c.name, c.duration_ms)
            })
            .collect();
        return format!(
            "{passed}/{} commands passed ({}) [tự chạy]",
            r.commands.len(),
            summary.join(", ")
        );
    }
    if let Some(g) = ctx.cg {
        let st = non_empty(&g.status).unwrap_or("UNKNOWN");
        return match (g.commands_passed, g.commands_total) {
            (Some(p), Some(t)) => {
                format!("compact GateResult: {st}, {p}/{t} commands passed [tự chạy]")
            }
            _ => format!("compact GateResult: {st} (worktree cleaned) [tự chạy]"),
        };
    }
    "chưa có biên nhận gate / chưa chạy test [tự chạy]".to_string()
}

fn build_new_bug_risk(ctx: &BuildCtx<'_>) -> String {
    if let Some(r) = ctx.receipt {
        let cnt = |sev: Severity| {
            r.static_findings
                .iter()
                .filter(|f| f.severity == sev)
                .count()
        };
        return format!(
            "static: {} critical, {} warn, {} info ({} suppressed); store compliance: {} ({} critical, {} warn, {} unresolved) [tự chạy]",
            cnt(Severity::Critical),
            cnt(Severity::Warn),
            cnt(Severity::Info),
            r.suppressions,
            enum_label(&r.store_compliance.status),
            r.store_compliance.critical,
            r.store_compliance.warnings,
            r.store_compliance.unresolved.len()
        );
    }
    if let Some(g) = ctx.cg {
        let unresolved = g
            .store_unresolved
            .as_u64()
            .or_else(|| {
                g.store_unresolved
                    .as_array()
                    .map(|a| u64::try_from(a.len()).unwrap_or(u64::MAX))
            })
            .unwrap_or(0);
        let st = non_empty(&g.store_status).unwrap_or("n/a");
        return format!(
            "compact static: {} critical, {} warn ({} suppressed); store: {st} ({} critical, {} warn, {unresolved} unresolved) [tự chạy]",
            g.static_critical.unwrap_or(0),
            g.static_warn.unwrap_or(0),
            g.suppressions.unwrap_or(0),
            g.store_critical.unwrap_or(0),
            g.store_warnings.unwrap_or(0)
        );
    }
    "chưa có dữ liệu so sánh static/store [tự chạy]".to_string()
}

fn build_code_safety(ctx: &BuildCtx<'_>) -> String {
    let mut parts = Vec::new();
    if let Some(r) = ctx.receipt {
        parts.push(format!(
            "head-lock: {} ({} locked change(s)) [tự chạy]",
            enum_label(&r.head_lock.status),
            r.head_lock.locked_changes.len()
        ));
    } else if let Some(lock) = ctx.cg.and_then(|g| non_empty(&g.head_lock_status)) {
        parts.push(format!("head-lock: {lock} [tự chạy]"));
    }
    parts.push(match (ctx.audit_verdict, ctx.audit_reviewer) {
        (Some(v), Some(rev)) => format!("cross-audit: {v} by {rev}"),
        (Some(v), None) => format!("cross-audit: {v}"),
        (None, Some(rev)) => format!("reviewer: {rev}"),
        (None, None) => "cross-audit: none".to_string(),
    });
    if ctx.bundle.same_account_warning {
        parts.push(SOLO_DEV_BADGE.to_string());
    }
    parts.push(if let Some(p) = ctx.proof {
        let short_sha: String = p.image_sha256.chars().take(12).collect();
        format!(
            "proof: verified ({}, {}x{}, sha256={short_sha}) [tự chạy]",
            p.device_serial, p.width, p.height
        )
    } else if ctx.proof_required {
        "proof: MISSING (required) [tự chạy]".to_string()
    } else {
        "proof: không yêu cầu".to_string()
    });
    parts.join("; ")
}

fn build_unverified(ctx: &BuildCtx<'_>, missing: &[String]) -> String {
    let mut gaps: Vec<String> = missing.to_vec();
    if let Some(r) = ctx.receipt {
        gaps.extend(
            r.store_compliance
                .unresolved
                .iter()
                .map(|i| format!("store unresolved: {i}")),
        );
        if r.status != GateStatus::Clear {
            gaps.extend(r.reasons.iter().map(|reason| format!("gate: {reason}")));
        }
    } else if let Some(g) = ctx.cg {
        gaps.extend(g.reasons.iter().map(|reason| format!("gate: {reason}")));
    }
    if ctx.bundle.same_account_warning {
        gaps.push(SOLO_DEV_BADGE.to_string());
    }
    if gaps.is_empty() {
        "không có".to_string()
    } else {
        gaps.join("; ")
    }
}

/// Loads task, events, and verified signed receipt/audit/proof artifacts from
#[path = "handover_load.rs"]
mod load;
pub use load::load_handover;
