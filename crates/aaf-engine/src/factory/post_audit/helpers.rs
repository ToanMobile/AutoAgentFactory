//! Diff-scope, verdict-verification, finding-collection, and infra-routing
//! helpers for [`super`].

use std::path::Path;

use aaf_domain::config::{AuditScope, CrossAuditConfig};
use aaf_domain::event::EventKind;
use aaf_domain::gate::{GateMode, GoldenScore, PostGateAuditRecord, Receipt, Severity};
use aaf_domain::ports::{InfraFailure, InfraKind, PortResult};
use serde_json::json;

use crate::cross_audit::{AuditVerdict, Finding, MechanicalScoreReport, evaluate_score_gate};
use crate::factory::audit_backend;
use crate::factory::ports::Grant;
use crate::factory::worker::{self, GateJob, Next};
use crate::factory::{Deps, unblock};
use crate::git::TaskWorktree;
use crate::store_compliance::PRE_EXISTING_PREFIX;

const LARGE_DIFF_MIN_FILES: usize = 3;
const LARGE_DIFF_MAX_PROD_LOC: usize = 200;

fn diff_header_path(line: &str) -> &str {
    if let Some((_, rhs)) = line
        .rsplit_once(" b/")
        .or_else(|| line.rsplit_once(" \"b/"))
    {
        rhs.trim_matches('"')
    } else {
        line.split_whitespace()
            .last()
            .unwrap_or("")
            .trim_matches('"')
            .trim_start_matches("b/")
    }
}

fn is_test_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    if normalized
        .split('/')
        .any(|c| matches!(c, "tests" | "test" | "__tests__" | "androidTest"))
    {
        return true;
    }
    let name = normalized.rsplit('/').next().unwrap_or(&normalized);
    name.starts_with("test_")
        || name.contains(".test.")
        || name.contains(".spec.")
        || name.rsplit_once('.').is_some_and(|(s, _)| {
            s.ends_with("_test") || s.ends_with("Test") || s.ends_with("Tests")
        })
}

/// Returns `true` when `diff` crosses the `AuditScope::Large` threshold:
/// truncated diff, `>= 3` changed files, or `> 200` changed production LOC
/// (excluding obvious test paths such as `tests/`, `*_test.*`, `*Test.*`).
pub fn is_large_diff(diff: &str, diff_truncated: bool) -> bool {
    if diff_truncated {
        return true;
    }
    let (mut changed_files, mut prod_loc_changed, mut current_is_test) = (0usize, 0usize, false);
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            changed_files += 1;
            current_is_test = is_test_path(diff_header_path(line));
        } else if !line.starts_with("+++ ")
            && !line.starts_with("--- ")
            && (line.starts_with('+') || line.starts_with('-'))
            && !current_is_test
        {
            prod_loc_changed += 1;
        }
    }
    changed_files >= LARGE_DIFF_MIN_FILES || prod_loc_changed > LARGE_DIFF_MAX_PROD_LOC
}

/// Inspects a worktree's diff against the base to decide `AuditScope::Large`.
/// Fails closed (`true`) if the diff cannot be computed.
pub(crate) fn is_large_worktree_diff(deps: &Deps, worktree: &TaskWorktree) -> bool {
    let Ok(diff) = deps.git.diff(worktree) else {
        return true;
    };
    let (_, truncated) = crate::cross_audit::prompt::truncate_diff(&diff);
    is_large_diff(&diff, truncated)
}

/// True when the receipt reports an oracle `behavioral_red_unproven` warning.
pub(crate) fn has_behavioral_red_unproven(receipt: &Receipt) -> bool {
    receipt
        .reasons
        .iter()
        .any(|r| r.contains("behavioral_red_unproven") || r.contains("behavioral red unproven"))
}

/// True when proof is not required for this diff or the signed proof for
/// `(worktree, tree_fp)` verifies.
pub(crate) fn proof_verified(deps: &Deps, worktree: &Path, receipt: &Receipt) -> bool {
    !crate::gate::proof_need::proof_needed_for_worktree(
        worktree,
        &deps.settings.repo.proof,
        &deps.settings.repo.gate.base_ref,
        receipt.base_sha.as_deref(),
    ) || crate::proof::verify_proof(
        worktree,
        &deps.settings.data_dir,
        &deps.settings.repo.proof,
        Some(&receipt.tree_fp),
    )
    .is_ok()
}

fn same_repo_root(a: &Path, b: &Path) -> bool {
    std::fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf())
        == std::fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf())
}

/// Loads and verifies the signed `"audit\0"` record bound to `(repo_root, tree_fp, config_hash)`.
pub(crate) fn load_verified_audit_verdict(
    deps: &Deps,
    repo_root: &Path,
    receipt: &Receipt,
) -> Option<PostGateAuditRecord> {
    let key = aaf_fp::load_key(&deps.settings.data_dir).ok()?;
    let record = aaf_fp::read_audit_verdict(
        &deps.settings.data_dir,
        &key,
        repo_root,
        &receipt.tree_fp,
        &receipt.config_hash,
    )
    .ok()
    .flatten()?;
    same_repo_root(Path::new(&record.repo_root), repo_root).then_some(record)
}

fn audit_satisfies_policy(record: &PostGateAuditRecord, cfg: &CrossAuditConfig) -> bool {
    if record.verdict != "PASS" {
        return false;
    }
    if !cfg.score_gate {
        return true;
    }
    let mech = MechanicalScoreReport {
        score: record.mechanical_score,
        reasons: Vec::new(),
        same_account_warning: false,
    };
    let eval = evaluate_score_gate(
        &mech,
        &record.scores,
        cfg.min_average_score,
        cfg.min_criterion_score,
    );
    eval.passed
        && record.effective_score >= cfg.min_average_score
        && record.llm_average == eval.llm_average
}

/// True when a `Verdict::Clear` task still requires a post-gate cross-audit
/// under `[cross_audit]` policy (or forced by oracle `behavioral_red_unproven`)
/// and does not yet have a signed `"PASS"` verdict covering
/// `(repo_root, tree_fp, config_hash)`.
pub(crate) fn needs_post_gate_audit(deps: &Deps, job: &GateJob, receipt: &Receipt) -> bool {
    let forced_behavioral = has_behavioral_red_unproven(receipt);
    let cfg = &deps.settings.repo.cross_audit;
    if !forced_behavioral && cfg.audit_scope == AuditScope::HeadLock && !cfg.score_gate {
        return false;
    }
    if let Some(record) = load_verified_audit_verdict(deps, &job.worktree.path, receipt)
        && audit_satisfies_policy(&record, cfg)
    {
        return false;
    }
    forced_behavioral
        || cfg.audit_scope == AuditScope::Always
        || cfg.score_gate
        || is_large_worktree_diff(deps, &job.worktree)
}

pub(crate) fn collect_rework_findings(
    verdict: &AuditVerdict,
    scores: &[GoldenScore],
    reasons: &[String],
    effective_score: u8,
    cfg: &CrossAuditConfig,
) -> Vec<Finding> {
    let mut actionable = match verdict {
        AuditVerdict::Fail { findings, .. } => findings.clone(),
        _ => Vec::new(),
    };
    if cfg.score_gate {
        let low_average = effective_score < cfg.min_average_score;
        let mut failing: Vec<&GoldenScore> = scores
            .iter()
            .filter(|s| s.score < cfg.min_criterion_score)
            .collect();
        failing.sort_by_key(|s| s.score);
        let mut deducted_passing: Vec<&GoldenScore> = if low_average {
            scores
                .iter()
                .filter(|s| {
                    s.score >= cfg.min_criterion_score
                        && s.score < cfg.min_average_score
                        && !s.deduction_reason.trim().is_empty()
                })
                .collect()
        } else {
            Vec::new()
        };
        deducted_passing.sort_by_key(|s| s.score);
        for s in failing.into_iter().chain(deducted_passing) {
            actionable.push(Finding {
                id: s.id.clone(),
                question: format!(
                    "score {}/100 (min criterion {})",
                    s.score, cfg.min_criterion_score
                ),
                evidence: s.deduction_reason.clone(),
            });
        }
        if actionable.is_empty() {
            for reason in reasons {
                actionable.push(Finding {
                    id: "score_gate".to_string(),
                    question: format!(
                        "effective score {effective_score}/100 (min average {})",
                        cfg.min_average_score
                    ),
                    evidence: reason.clone(),
                });
            }
        }
    }
    actionable
}

pub(super) fn collect_mechanical_findings(
    receipt: &Receipt,
    mech: &MechanicalScoreReport,
    min_average: u8,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    for item in &receipt.static_findings {
        if !matches!(item.severity, Severity::Warn | Severity::Critical) {
            continue;
        }
        let loc = item
            .line
            .map_or_else(|| item.file.clone(), |l| format!("{}:{l}", item.file));
        let evidence = if item.fix.trim().is_empty() {
            loc
        } else {
            format!("{loc}; fix: {}", item.fix)
        };
        findings.push(Finding {
            id: item.rule.clone(),
            question: item.message.clone(),
            evidence,
        });
    }
    for unresolved in &receipt.store_compliance.unresolved {
        if receipt.mode == GateMode::Scoped && unresolved.starts_with(PRE_EXISTING_PREFIX) {
            continue;
        }
        findings.push(Finding {
            id: "score_gate".to_string(),
            question: "unresolved store compliance item".to_string(),
            evidence: unresolved.clone(),
        });
    }
    let q = format!(
        "mechanical score {}/100 (min average {min_average})",
        mech.score
    );
    for reason in &mech.reasons {
        findings.push(Finding {
            id: "score_gate".to_string(),
            question: q.clone(),
            evidence: reason.clone(),
        });
    }
    if findings.is_empty() {
        findings.push(Finding {
            id: "score_gate".to_string(),
            question: q,
            evidence: "mechanical score below configured threshold".to_string(),
        });
    }
    findings
}

/// An explicit post-gate audit failure already recorded during the task's
/// current `Gating` visit before routing completed (crash / service restart).
pub(crate) struct InterruptedPostGateFail {
    pub(crate) event_id: Option<i64>,
    pub(crate) verdict: String,
    pub(crate) findings: Vec<Finding>,
}

const CRASH_REPLAY_SCAN_LIMIT: u32 = 50;

/// Detects an interrupted post-gate `CrossAudit` FAIL from the current `Gating`
/// visit for the exact `(tree_fp, config_hash)` so a crash/restart mid-routing
/// does not burn another identical LLM audit turn.
///
/// Stops immediately at any `ReworkRequested` or `TaskStateChanged` event so
/// completed rework cycles, human unblocks, or earlier `Gating` visits are
/// never replayed. Requires non-empty structured findings so rework guidance is
/// never degraded.
pub(crate) fn find_interrupted_post_gate_fail(
    deps: &Deps,
    task_id: &str,
    receipt: &Receipt,
    cfg: &CrossAuditConfig,
) -> PortResult<Option<InterruptedPostGateFail>> {
    let kinds = [
        EventKind::CrossAudit,
        EventKind::TaskStateChanged,
        EventKind::ReworkRequested,
    ];
    for event in deps
        .store
        .events_for_task_by_kinds(task_id, &kinds, CRASH_REPLAY_SCAN_LIMIT)?
    {
        if event.kind != EventKind::CrossAudit {
            // Any state transition (including entering Gating, moving to Ready/Blocked,
            // or human unblock) or ReworkRequested ends the current unrouted window.
            break;
        }
        let Ok(p) = serde_json::from_str::<serde_json::Value>(&event.payload_json) else {
            continue;
        };
        if p["phase"].as_str() != Some("post_gate") {
            continue;
        }
        if p.get("reused_from").is_some_and(|v| !v.is_null())
            || p.get("retry").and_then(serde_json::Value::as_bool) == Some(true)
            || p.get("short_circuited")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
        {
            continue;
        }
        let verdict = p["verdict"].as_str().unwrap_or("");
        if verdict == "auth_fallback" || verdict == "auth_wait" {
            continue;
        }
        if p["tree_fp"].as_str() != Some(receipt.tree_fp.as_str())
            || p["config_hash"].as_str() != Some(receipt.config_hash.as_str())
        {
            return Ok(None);
        }
        if verdict != "fail" && !verdict.starts_with("fail:") {
            return Ok(None);
        }
        let mut findings: Vec<Finding> = p
            .get("findings")
            .and_then(|v| serde_json::from_value::<Vec<Finding>>(v.clone()).ok())
            .unwrap_or_default();
        if findings.is_empty() && cfg.score_gate {
            let scores: Vec<GoldenScore> = p
                .get("scores")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            let reasons: Vec<String> = p
                .get("reasons")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            let effective = p["effective_score"]
                .as_u64()
                .and_then(|n| u8::try_from(n).ok())
                .unwrap_or(0);
            findings = collect_rework_findings(
                &AuditVerdict::Pass { warnings: vec![] },
                &scores,
                &reasons,
                effective,
                cfg,
            );
        }
        if findings.is_empty() {
            return Ok(None);
        }
        return Ok(Some(InterruptedPostGateFail {
            event_id: event.id,
            verdict: verdict.to_string(),
            findings,
        }));
    }
    Ok(None)
}

pub(super) fn reject_post_audit(
    deps: &Deps,
    task_id: String,
    worktree: TaskWorktree,
    reason: &str,
) -> PortResult<Next> {
    reject_post_audit_with_findings(deps, task_id, worktree, reason, &[])
}

pub(super) fn reject_post_audit_with_findings(
    deps: &Deps,
    task_id: String,
    worktree: TaskWorktree,
    reason: &str,
    findings: &[Finding],
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
    let signal = if reason.contains("diff was truncated") {
        audit_backend::audit_rejection_signal(&format!("audit_diff_truncated: {reason}"))
    } else if reason.starts_with("inconclusive:") {
        audit_backend::audit_rejection_signal(reason)
    } else {
        let mut s =
            audit_backend::audit_rejection_signal("post-gate cross-audit rejected the changes");
        s.audit_rejected = Some(unblock::one_line(reason, 200));
        s
    };
    Ok(Next::Unblock {
        task_id,
        worktree,
        signal: Box::new(signal),
    })
}

fn secondary_provider_configured(deps: &Deps) -> bool {
    let run_exec = &deps.settings.repo.run.executor;
    let audit_exec =
        audit_backend::configured_audit_provider(run_exec, &deps.settings.repo.cross_audit);
    !audit_backend::same_audit_provider(run_exec, audit_exec)
}

pub(super) fn primary_infra_next(
    deps: &Deps,
    task_id: &str,
    grant: &Grant,
    infra: &InfraFailure,
) -> Next {
    let (what, fix) = worker::infra_words(grant, infra);
    let detail = worker::halt_message(task_id, what, infra, &fix);
    if infra.kind == InfraKind::Network {
        Next::RetryAfter {
            until: deps.now(),
            attempt: 1,
            detail,
        }
    } else {
        Next::Halt(detail)
    }
}

const MAX_AUTH_WAITS: usize = 3;
const AUTH_WAIT_HISTORY: u32 = 200;

fn count_auth_waits(deps: &Deps, task_id: &str) -> PortResult<usize> {
    let kinds = [
        EventKind::CrossAudit,
        EventKind::TaskStateChanged,
        EventKind::ReworkRequested,
    ];
    let mut count = 0usize;
    for event in deps
        .store
        .events_for_task_by_kinds(task_id, &kinds, AUTH_WAIT_HISTORY)?
    {
        if event.kind == EventKind::ReworkRequested {
            break;
        }
        if event.kind == EventKind::TaskStateChanged {
            let to = serde_json::from_str::<serde_json::Value>(&event.payload_json)
                .ok()
                .and_then(|p| p["to"].as_str().map(str::to_string));
            if matches!(to.as_deref(), Some("blocked" | "done")) {
                break;
            }
            continue;
        }
        if event.kind == EventKind::CrossAudit
            && serde_json::from_str::<serde_json::Value>(&event.payload_json)
                .ok()
                .is_some_and(|p| p["phase"].as_str() == Some("auth_wait"))
        {
            count += 1;
        }
    }
    Ok(count)
}

/// Maps an auditor infrastructure failure to [`Next::RetryAfter`] for transient
/// network faults, bounds secondary audit provider auth waits to
/// [`MAX_AUTH_WAITS`] before calling [`unblock::block_untried`], and returns
/// [`Next::Halt`] for primary/required authentication failures.
pub(crate) fn audit_infra_next(
    deps: &Deps,
    task_id: &str,
    grant: &Grant,
    infra: &InfraFailure,
) -> PortResult<Next> {
    if infra.kind == InfraKind::Auth
        && !deps.settings.repo.cross_audit.require_different_provider
        && secondary_provider_configured(deps)
    {
        let provider = audit_backend::configured_audit_provider(
            &deps.settings.repo.run.executor,
            &deps.settings.repo.cross_audit,
        );
        deps.emit(
            EventKind::CrossAudit,
            task_id,
            json!({
                "phase": "auth_wait",
                "verdict": "auth_wait",
                "provider": provider,
                "why": unblock::one_line(&infra.raw, 300),
            }),
        )?;
        if count_auth_waits(deps, task_id)? >= MAX_AUTH_WAITS {
            let (_, fix) = worker::infra_words(grant, infra);
            let why = format!(
                "auditor {provider} cannot log in: {fix}; then `aaf board unblock {task_id}`"
            );
            unblock::block_untried(deps, task_id, &why)?;
            return Ok(Next::Blocked);
        }
        return Ok(Next::Wait {
            until: Some(deps.now() + 15),
        });
    }
    Ok(primary_infra_next(deps, task_id, grant, infra))
}
