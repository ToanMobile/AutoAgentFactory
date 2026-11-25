//! Handover report section builders (5-item acceptance fields, DevKit B10
//! summary lines, root cause extraction, and gate command snapshots).

use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::gate::{GateStatus, GoldenScore, PostGateAuditRecord, Receipt, Severity};
use aaf_domain::proof::ProofRecord;
use aaf_domain::task::{Task, TaskState};
use serde_json::Value;

use super::{
    CompactAudit, CompactGate, HandoverGateCommand, SOLO_DEV_BADGE, enum_label, latest_event,
    non_empty,
};
use crate::cross_audit::scoring::compute_mechanical_score;
use crate::handover_html::HandoverProof;

pub(super) struct ScoreBundle {
    pub(super) mechanical: Option<u8>,
    pub(super) llm_average: Option<u8>,
    pub(super) effective: Option<u8>,
    pub(super) scores: Vec<GoldenScore>,
    pub(super) same_account_warning: bool,
}

pub(super) struct BuildCtx<'a> {
    pub(super) task: &'a Task,
    pub(super) receipt: Option<&'a Receipt>,
    pub(super) cg: Option<&'a CompactGate>,
    pub(super) audit: Option<&'a PostGateAuditRecord>,
    pub(super) ca: Option<&'a CompactAudit>,
    pub(super) proof: Option<&'a ProofRecord>,
    pub(super) proof_required: bool,
    pub(super) audit_verdict: Option<&'a str>,
    pub(super) audit_reviewer: Option<&'a str>,
    pub(super) bundle: ScoreBundle,
}

pub(super) fn resolve_scores(
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

pub(super) fn build_prevent_old_bug(ctx: &BuildCtx<'_>) -> String {
    if let Some(r) = ctx.receipt {
        if r.commands.is_empty() {
            return "no gate commands recorded [tự chạy]".to_string();
        }
        let passed = r.commands.iter().filter(|c| c.passed()).count();
        let summary: Vec<String> = r
            .commands
            .iter()
            .map(|c| {
                let st = match (c.passed(), c.timed_out) {
                    (true, _) => "PASS",
                    (false, true) => "TIMEOUT",
                    (false, false) => "FAIL",
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

pub(super) fn build_new_bug_risk(ctx: &BuildCtx<'_>) -> String {
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

pub(super) fn build_code_safety(ctx: &BuildCtx<'_>) -> String {
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

pub(super) fn build_unverified(ctx: &BuildCtx<'_>, missing: &[String]) -> String {
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

pub(super) fn verdict_label_for(status: &str) -> String {
    if status == "XONG" {
        "PASS — ĐỦ ĐIỀU KIỆN NGHIỆM THU & BÀN GIAO".to_string()
    } else {
        "CHƯA ĐẠT — CÒN THIẾU ĐIỀU KIỆN NGHIỆM THU".to_string()
    }
}

pub(super) fn map_gate_commands(receipt: Option<&Receipt>) -> Vec<HandoverGateCommand> {
    let Some(r) = receipt else {
        return Vec::new();
    };
    r.commands
        .iter()
        .map(|c| HandoverGateCommand {
            name: c.name.clone(),
            command: c.command.clone(),
            exit_code: c.exit_code,
            timed_out: c.timed_out,
            duration_ms: c.duration_ms,
        })
        .collect()
}

fn one_line(s: &str, max_chars: usize) -> String {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = collapsed.chars().take(max_chars).collect();
    if collapsed.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

fn find_root_cause_in_text(text: &str) -> Option<String> {
    for raw_line in text.lines() {
        for segment in raw_line.split(" · ") {
            let trimmed = segment
                .trim()
                .trim_start_matches(['-', '*', '>'])
                .trim()
                .trim_start_matches("**")
                .trim();
            let lower = trimmed.to_lowercase();
            let prefix_len = if lower.starts_with("nguyên nhân:") {
                "nguyên nhân:".len()
            } else if lower.starts_with("root cause:") {
                "root cause:".len()
            } else {
                continue;
            };
            // Slice by matching char count on original string (safe for UTF-8 case folding).
            let char_skip = lower[..prefix_len].chars().count();
            let rest: String = trimmed.chars().skip(char_skip).collect();
            let cleaned = rest.trim().trim_start_matches("**").trim();
            let line = one_line(cleaned, 240);
            if !line.is_empty() {
                return Some(line);
            }
        }
    }
    None
}

fn extract_event_summary_text(events: &[FactoryEvent], kind: EventKind) -> Option<String> {
    let ev = latest_event(events, kind)?;
    let val: Value = serde_json::from_str(&ev.payload_json).ok()?;
    val.get("plan_contract")
        .and_then(|c| c.get("summary"))
        .and_then(Value::as_str)
        .or_else(|| val.get("summary").and_then(Value::as_str))
        .or_else(|| val.get("plan").and_then(Value::as_str))
        .map(str::to_string)
}

pub(super) fn extract_root_cause(task: &Task, events: &[FactoryEvent], what_done: &str) -> String {
    for note in task.context_notes.iter().rev() {
        if let Some(found) = find_root_cause_in_text(note) {
            return found;
        }
    }
    for kind in [EventKind::PlanConverged, EventKind::PlanGenerated] {
        if let Some(summary) = extract_event_summary_text(events, kind)
            && let Some(found) = find_root_cause_in_text(&summary)
        {
            return found;
        }
    }
    if let Some(found) = find_root_cause_in_text(what_done) {
        return found;
    }
    if let Some(found) = find_root_cause_in_text(&task.spec.description) {
        return found;
    }
    let desc = one_line(&task.spec.description, 200);
    if !desc.is_empty() {
        format!("Theo yêu cầu task: {desc}")
    } else {
        format!("Theo yêu cầu task: {}", one_line(&task.title, 200))
    }
}

pub(super) fn build_user_outcome(task: &Task, what_done: &str) -> String {
    let title = one_line(&task.title, 120);
    let detail = what_done
        .split(" · ")
        .skip(1)
        .find(|seg| {
            let s = seg.trim();
            s.starts_with("Plan:") || s.starts_with("Notes:") || s.starts_with("Spec:")
        })
        .map(|seg| {
            seg.trim()
                .trim_start_matches("Plan:")
                .trim_start_matches("Notes:")
                .trim_start_matches("Spec:")
                .trim()
        })
        .filter(|s| !s.is_empty());
    match detail {
        Some(extra) => one_line(&format!("{title} — {extra}"), 220),
        None => {
            let fallback = one_line(what_done, 220);
            if fallback.is_empty() { title } else { fallback }
        }
    }
}

pub(super) fn build_next_step(task: &Task, status: &str, missing: &[String]) -> String {
    if status == "XONG" {
        return match task.state {
            TaskState::Done => "Đã bàn giao (Done)".to_string(),
            TaskState::Review => "Sẵn sàng merge (aaf merge)".to_string(),
            _ => "Sẵn sàng kiểm tra bàn giao".to_string(),
        };
    }
    if missing.is_empty() {
        "Kiểm tra lại điều kiện nghiệm thu".to_string()
    } else {
        format!("Cần xử lý: {}", one_line(&missing.join("; "), 200))
    }
}

pub(super) fn build_gate_proof_next_line(
    ctx: &BuildCtx<'_>,
    tree_fp: Option<&str>,
    proof: Option<&HandoverProof>,
    next_step: &str,
) -> String {
    let gate_lbl = match ctx.receipt {
        Some(r) if r.status == GateStatus::Clear => "PASS/exit 0",
        Some(_) => "FAIL",
        None => match ctx.cg.and_then(|g| non_empty(&g.status)) {
            Some(s) if s.eq_ignore_ascii_case("clear") => "PASS/exit 0",
            Some(_) => "FAIL",
            None => "CHƯA CHẠY",
        },
    };
    let fp_lbl = tree_fp.unwrap_or("n/a");
    let proof_lbl = match proof {
        Some(p) => p.summary.clone(),
        None if !ctx.proof_required => "không cần — proof.required = false".to_string(),
        None => "CHƯA CÓ".to_string(),
    };
    format!("gate: {gate_lbl} (tree {fp_lbl}) · ảnh: {proof_lbl} · bước tiếp theo: {next_step}")
}
