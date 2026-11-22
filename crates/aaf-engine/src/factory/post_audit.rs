//! Post-gate adversarial cross-audit with optional 7 Golden Standards score
//! gate and HMAC-signed `"audit\0"` verdicts bound to `(repo_root, tree_fp, config_hash)`.

use std::path::Path;

use aaf_domain::config::{AuditScope, CrossAuditConfig};
use aaf_domain::event::EventKind;
use aaf_domain::gate::{PostGateAuditRecord, Receipt};
use aaf_domain::ports::{PortError, PortResult};
use serde_json::json;

use super::audit_backend::{self, AuditBackend};
use super::ports::{AccountPool, Grant};
use super::worker::{self, GateJob, Next, Outcome};
use super::{Deps, unblock};
use crate::cross_audit::{
    AuditInput, AuditVerdict, Finding, MechanicalScoreReport, compute_mechanical_score,
    evaluate_score_gate,
};
use crate::git::TaskWorktree;

const LARGE_DIFF_MIN_FILES: usize = 3;
const LARGE_DIFF_MAX_PROD_LOC: usize = 200;

fn diff_header_path(line: &str) -> &str {
    if let Some((_, rhs)) = line.rsplit_once(" b/") {
        rhs.trim_matches('"')
    } else if let Some((_, rhs)) = line.rsplit_once(" \"b/") {
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
    for component in normalized.split('/') {
        if matches!(component, "tests" | "test" | "__tests__" | "androidTest") {
            return true;
        }
    }
    let file_name = normalized.rsplit('/').next().unwrap_or(&normalized);
    if file_name.starts_with("test_")
        || file_name.contains(".test.")
        || file_name.contains(".spec.")
    {
        return true;
    }
    if let Some((stem, _ext)) = file_name.rsplit_once('.')
        && (stem.ends_with("_test") || stem.ends_with("Test") || stem.ends_with("Tests"))
    {
        return true;
    }
    false
}

/// Returns `true` when `diff` crosses the `AuditScope::Large` threshold:
/// truncated diff, `>= 3` changed files, or `> 200` changed production LOC
/// (excluding obvious test paths such as `tests/`, `*_test.*`, `*Test.*`).
pub fn is_large_diff(diff: &str, diff_truncated: bool) -> bool {
    if diff_truncated {
        return true;
    }
    let mut changed_files: usize = 0;
    let mut prod_loc_changed: usize = 0;
    let mut current_is_test = false;

    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            changed_files += 1;
            current_is_test = is_test_path(diff_header_path(line));
            continue;
        }
        if line.starts_with("+++ ") || line.starts_with("--- ") {
            continue;
        }
        if (line.starts_with('+') || line.starts_with('-')) && !current_is_test {
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

/// True when proof is not required or the signed proof for `(worktree, tree_fp)` verifies.
pub(crate) fn proof_verified(deps: &Deps, worktree: &Path, receipt: &Receipt) -> bool {
    if !deps.settings.repo.proof.required {
        return true;
    }
    crate::proof::verify_proof(
        worktree,
        &deps.settings.data_dir,
        &deps.settings.repo.proof,
        Some(&receipt.tree_fp),
    )
    .is_ok()
}

fn same_repo_root(a: &Path, b: &Path) -> bool {
    let ca = std::fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let cb = std::fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    ca == cb
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
    if !same_repo_root(Path::new(&record.repo_root), repo_root) {
        return None;
    }
    Some(record)
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
/// under `[cross_audit]` policy and does not yet have a signed `"PASS"` verdict
/// covering `(repo_root, tree_fp, config_hash)`.
pub(crate) fn needs_post_gate_audit(deps: &Deps, job: &GateJob, receipt: &Receipt) -> bool {
    let cfg = &deps.settings.repo.cross_audit;
    if cfg.audit_scope == AuditScope::HeadLock && !cfg.score_gate {
        return false;
    }
    if let Some(record) = load_verified_audit_verdict(deps, &job.worktree.path, receipt)
        && audit_satisfies_policy(&record, cfg)
    {
        return false;
    }
    cfg.audit_scope == AuditScope::Always
        || cfg.score_gate
        || is_large_worktree_diff(deps, &job.worktree)
}

fn collect_rework_findings(
    verdict: &AuditVerdict,
    scores: &[aaf_domain::gate::GoldenScore],
    reasons: &[String],
    effective_score: u8,
    cfg: &CrossAuditConfig,
) -> Vec<Finding> {
    let mut actionable = match verdict {
        AuditVerdict::Fail { findings, .. } => findings.clone(),
        _ => Vec::new(),
    };
    if cfg.score_gate {
        for s in scores {
            if s.score < cfg.min_criterion_score
                || (!s.deduction_reason.trim().is_empty() && s.score < 100)
            {
                actionable.push(Finding {
                    id: s.id.clone(),
                    question: format!(
                        "score {}/100 (min criterion {})",
                        s.score, cfg.min_criterion_score
                    ),
                    evidence: s.deduction_reason.clone(),
                });
            }
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

/// Runs the post-gate cross-audit worker job and maps any infrastructure error
/// to [`worker::block_infra`].
pub(crate) fn run_post_gate_audit(
    deps: &Deps,
    audit_backend: Option<&AuditBackend>,
    job: GateJob,
    receipt: Receipt,
    grant: &Grant,
) -> Outcome {
    let task_id = job.task_id.clone();
    match execute_post_gate_audit(deps, audit_backend, job, receipt, grant) {
        Ok(next) => Outcome {
            task_id,
            next,
            error: None,
        },
        Err(err) => worker::block_infra(deps, &task_id, &err.to_string()),
    }
}

fn execute_post_gate_audit(
    deps: &Deps,
    audit_backend: Option<&AuditBackend>,
    job: GateJob,
    receipt: Receipt,
    grant: &Grant,
) -> PortResult<Next> {
    let task = deps.store.get_task(&job.task_id)?;
    let (audit_provider, reviewer, same_account) =
        match audit_backend::resolve_task_audit_context(deps, audit_backend, &task.id, grant)? {
            Ok(ctx) => ctx,
            Err(why) => {
                unblock::block_untried(deps, &task.id, &why)?;
                return Ok(Next::Blocked);
            }
        };
    let input = AuditInput {
        spec: task.spec.clone(),
        diff: deps.git.diff(&job.worktree)?,
        mobile: deps.settings.mobile,
    };
    let cfg = &deps.settings.repo.cross_audit;

    let clean = deps.git.integrate(&[])?;
    let audited = audit_backend::run_auditor_turn(
        deps,
        audit_backend,
        &task,
        &input,
        &clean.tree,
        grant,
        cfg.score_gate,
    );
    let _ = deps.git.discard(&clean);
    let (report, infra, limit_hit) = audited?;
    if let Some(hit) = limit_hit {
        let accounts: &dyn AccountPool = audit_backend.map_or(&*deps.accounts, |a| &*a.accounts);
        audit_backend::report_audit_limit(deps, accounts, &task.id, grant, &hit)?;
        return audit_backend::route_audit_limit(accounts, grant, &hit);
    }
    if let Some(infra) = infra {
        let (what, fix) = worker::infra_words(grant, &infra);
        return Ok(Next::Halt(worker::halt_message(
            &task.id, what, &infra, &fix,
        )));
    }

    let proof_ok = proof_verified(deps, &job.worktree.path, &receipt);
    let mech = compute_mechanical_score(
        &receipt,
        proof_ok,
        cfg.require_different_provider,
        same_account,
    );
    let eval = cfg.score_gate.then(|| {
        evaluate_score_gate(
            &mech,
            &report.scores,
            cfg.min_average_score,
            cfg.min_criterion_score,
        )
    });
    let llm_average = eval.as_ref().and_then(|e| e.llm_average).or_else(|| {
        (report.scores.len() == 7).then(|| {
            let sum: u32 = report.scores.iter().map(|s| u32::from(s.score)).sum();
            u8::try_from(sum / 7).unwrap_or(100)
        })
    });
    let effective_score = eval.as_ref().map_or_else(
        || llm_average.map_or(mech.score, |avg| mech.score.min(avg)),
        |e| e.effective_score,
    );
    let mut reasons = mech.reasons.clone();
    if let Some(ref e) = eval {
        reasons.extend(e.reasons.iter().cloned());
    }

    let passed = report.verdict.is_pass()
        && !report.diff_truncated
        && eval.as_ref().is_none_or(|e| e.passed);
    let score_gate_failed = report.verdict.is_pass()
        && !report.diff_truncated
        && eval
            .as_ref()
            .is_some_and(|e| !e.passed && e.llm_average.is_some());
    let event_verdict = if passed {
        "pass".to_string()
    } else if score_gate_failed {
        format!("fail: {}", reasons.join("; "))
    } else {
        audit_backend::verdict_text(&report.verdict)
    };

    deps.emit(
        EventKind::CrossAudit,
        &task.id,
        json!({
            "phase": "post_gate",
            "verdict": event_verdict,
            "provider": audit_provider,
            "reviewer": reviewer,
            "same_account": same_account,
            "mechanical_score": mech.score,
            "llm_average": llm_average,
            "effective_score": effective_score,
            "scores": report.scores,
            "reasons": reasons,
            "tree_fp": receipt.tree_fp,
            "config_hash": receipt.config_hash,
        }),
    )?;

    if passed {
        let canon =
            std::fs::canonicalize(&job.worktree.path).unwrap_or_else(|_| job.worktree.path.clone());
        let record = PostGateAuditRecord {
            repo_root: canon.to_string_lossy().into_owned(),
            tree_fp: receipt.tree_fp,
            config_hash: receipt.config_hash,
            verdict: "PASS".to_string(),
            reviewer,
            same_account,
            mechanical_score: mech.score,
            llm_average,
            effective_score,
            scores: report.scores,
            created_at: deps.now(),
        };
        let io = |err: aaf_fp::FpError| PortError::Io(err.to_string());
        let key = aaf_fp::load_or_create_key(&deps.settings.data_dir).map_err(io)?;
        aaf_fp::write_audit_verdict(&deps.settings.data_dir, &key, &job.worktree.path, &record)
            .map_err(io)?;
        return Ok(Next::Gate(GateJob {
            audited: true,
            ..job
        }));
    }

    let explicit_fail = !report.diff_truncated
        && (matches!(report.verdict, AuditVerdict::Fail { .. }) || score_gate_failed);
    if explicit_fail {
        let findings = collect_rework_findings(
            &report.verdict,
            &report.scores,
            &reasons,
            effective_score,
            cfg,
        );
        if let Some(next) = unblock::handle_audit_failure(deps, &task, &findings)? {
            return Ok(next);
        }
    }

    let note = format!("post-gate cross-audit rejected the changes ({event_verdict})");
    deps.store
        .append_context_note(&task.id, &unblock::one_line(&note, 400), deps.now())?;
    let signal =
        audit_backend::audit_rejection_signal("post-gate cross-audit rejected the changes");
    Ok(Next::Unblock {
        task_id: task.id,
        worktree: job.worktree,
        signal: Box::new(signal),
    })
}
