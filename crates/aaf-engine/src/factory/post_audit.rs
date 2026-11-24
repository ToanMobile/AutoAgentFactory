//! Post-gate adversarial cross-audit with optional 7 Golden Standards score
//! gate and HMAC-signed `"audit\0"` verdicts bound to `(repo_root, tree_fp, config_hash)`.

use std::path::Path;

use aaf_domain::config::{AuditScope, CrossAuditConfig};
use aaf_domain::event::EventKind;
use aaf_domain::gate::{PostGateAuditRecord, Receipt};
use aaf_domain::ports::{InfraKind, PortError, PortResult};
use serde_json::json;

#[allow(dead_code)]
#[path = "post_audit_helpers.rs"]
mod helpers;

pub use helpers::is_large_diff;
pub(crate) use helpers::{
    audit_infra_next, collect_rework_findings, has_behavioral_red_unproven, is_large_worktree_diff,
    load_verified_audit_verdict, proof_verified,
};
use helpers::{collect_mechanical_findings, primary_infra_next, reject_post_audit};

use super::audit_backend::{self, AuditBackend};
use super::audit_turn::short_circuit_audit_reason;
use super::ports::{AccountPool, Grant, Purpose};
use super::rework_filter;
use super::stage_router::ProviderKey;
use super::worker::{self, GateJob, Job, Next, Outcome, PreGateAudit};
use super::{Deps, unblock};
use crate::cross_audit::{
    AuditInput, AuditVerdict, GOLDEN_IDS, MechanicalScoreReport, compute_mechanical_score,
    evaluate_score_gate,
};

const BEHAVIORAL_RED_UNPROVEN_CHECKLIST_NOTE: &str = "Oracle behavioral_red_unproven: verify that the \
     new module's tests contain non-vacuous behavioral assertions exercising real production logic \
     and edge cases";

/// Runs the post-gate cross-audit worker job and maps any infrastructure error
/// to [`worker::block_infra`].
pub(crate) fn run_post_gate_audit(
    deps: &Deps,
    audit_backend: Option<&AuditBackend>,
    job: GateJob,
    receipt: Receipt,
    pre_audit: Option<Box<PreGateAudit>>,
    grant: &Grant,
) -> Outcome {
    let task_id = job.task_id.clone();
    match execute_post_gate_audit(deps, audit_backend, job, receipt, pre_audit, grant) {
        Ok(next) => Outcome {
            task_id,
            provider: ProviderKey::from_executor("default"),
            next,
            error: None,
        },
        Err(err) => worker::block_infra(deps, &task_id, &err.to_string()),
    }
}

fn mech_score(deps: &Deps, tree: &Path, receipt: &Receipt, same: bool) -> MechanicalScoreReport {
    let cfg = &deps.settings.repo.cross_audit;
    compute_mechanical_score(
        receipt,
        proof_verified(deps, tree, receipt),
        cfg.require_different_provider,
        same,
    )
}

fn acquire_primary_fallback_grant(deps: &Deps, task_id: &str) -> PortResult<Option<Grant>> {
    if let Some(grant) = deps.accounts.acquire_for(task_id, Purpose::Audit)? {
        return Ok(Some(grant));
    }
    Ok(deps
        .accounts
        .profiles()?
        .is_empty()
        .then(|| Grant::new(None, None)))
}

fn audit_satisfies_policy(record: &PostGateAuditRecord, cfg: &CrossAuditConfig) -> bool {
    if cfg.dual_review {
        return super::dual_audit::dual_audit_satisfies_policy(record, cfg);
    }
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
    if !forced_behavioral
        && !cfg.dual_review
        && cfg.audit_scope == AuditScope::HeadLock
        && !cfg.score_gate
    {
        return false;
    }
    if let Some(record) = load_verified_audit_verdict(deps, &job.worktree.path, receipt)
        && audit_satisfies_policy(&record, cfg)
    {
        return false;
    }
    forced_behavioral
        || cfg.dual_review
        || cfg.audit_scope == AuditScope::Always
        || cfg.score_gate
        || is_large_worktree_diff(deps, &job.worktree)
}

fn can_reuse_pre_audit(
    pre: &PreGateAudit,
    receipt: &Receipt,
    cfg: &CrossAuditConfig,
    forced_behavioral: bool,
) -> bool {
    // §D.4.1: PreGateAudit MUST NEVER bypass Dual-Review.
    if forced_behavioral || cfg.dual_review {
        return false;
    }
    !pre.tree_fp.is_empty()
        && pre.tree_fp == receipt.tree_fp
        && pre.base_sha == receipt.base_sha
        && (!cfg.score_gate || pre.scores.len() == GOLDEN_IDS.len())
}

fn execute_post_gate_audit(
    deps: &Deps,
    audit_backend: Option<&AuditBackend>,
    job: GateJob,
    receipt: Receipt,
    pre_audit: Option<Box<PreGateAudit>>,
    grant: &Grant,
) -> PortResult<Next> {
    let task = deps.store.get_task(&job.task_id)?;
    let cfg = &deps.settings.repo.cross_audit;
    let forced_behavioral = has_behavioral_red_unproven(&receipt);
    let reusable =
        pre_audit.filter(|pre| can_reuse_pre_audit(pre, &receipt, cfg, forced_behavioral));

    let (audit_provider, reviewer, same_account, pre_mech, verdict, diff_truncated, scores) =
        if let Some(pre) = reusable {
            (
                pre.audit_provider,
                pre.reviewer,
                pre.same_account,
                None,
                AuditVerdict::Pass { warnings: vec![] },
                false,
                pre.scores,
            )
        } else {
            let (mut audit_provider, mut reviewer, mut same_account) =
                match audit_backend::resolve_task_audit_context(
                    deps,
                    audit_backend,
                    &task.id,
                    grant,
                )? {
                    Ok(ctx) => ctx,
                    Err(why) => {
                        unblock::block_untried(deps, &task.id, &why)?;
                        return Ok(Next::Blocked);
                    }
                };
            let mut spec = task.spec.clone();
            if forced_behavioral {
                spec.acceptance
                    .push(BEHAVIORAL_RED_UNPROVEN_CHECKLIST_NOTE.to_string());
            }
            let input = AuditInput {
                spec,
                diff: deps.git.diff(&job.worktree)?,
                mobile: deps.settings.mobile,
            };
            if let Some(reason) = short_circuit_audit_reason(&input.diff) {
                deps.emit(EventKind::CrossAudit, &task.id, json!({
                    "phase": "post_gate", "verdict": reason, "provider": audit_provider,
                    "reviewer": reviewer, "same_account": same_account,
                    "tree_fp": receipt.tree_fp, "config_hash": receipt.config_hash, "short_circuited": true,
                }))?;
                return reject_post_audit(deps, task.id, job.worktree, &reason);
            }
            let mut pre_mech = cfg
                .score_gate
                .then(|| mech_score(deps, &job.worktree.path, &receipt, same_account));
            if let Some(ref mech) = pre_mech
                && mech.score < cfg.min_average_score
            {
                let mut reasons = mech.reasons.clone();
                reasons.push(format!(
                    "mechanical score {} is below minimum {}",
                    mech.score, cfg.min_average_score
                ));
                let event_verdict = format!("fail: {}", reasons.join("; "));
                deps.emit(EventKind::CrossAudit, &task.id, json!({
                    "phase": "post_gate", "verdict": event_verdict, "provider": audit_provider,
                    "reviewer": reviewer, "same_account": same_account, "mechanical_score": mech.score,
                    "llm_average": Option::<u8>::None, "effective_score": mech.score,
                    "scores": serde_json::Value::Array(Vec::new()), "reasons": reasons,
                    "tree_fp": receipt.tree_fp, "config_hash": receipt.config_hash, "short_circuited": true,
                }))?;
                let findings = collect_mechanical_findings(&receipt, mech, cfg.min_average_score);
                if let Some(next) = unblock::handle_audit_failure(deps, &task, &findings)? {
                    return Ok(next);
                }
                return reject_post_audit(deps, task.id, job.worktree, &event_verdict);
            }
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
            let (mut report, mut infra, mut limit_hit) = match audited {
                Ok(res) => res,
                Err(err) => {
                    let _ = deps.git.discard(&clean);
                    return Err(err);
                }
            };
            let mut active_accounts: &dyn AccountPool =
                audit_backend.map_or(&*deps.accounts, |a| &*a.accounts);
            let mut fallback_grant_slot: Option<Grant> = None;
            let mut tier1_blocked_fallback = false;
            if audit_backend.is_some()
                && !cfg.require_different_provider
                && infra.as_ref().is_some_and(|i| i.kind == InfraKind::Auth)
                && let Some(fb_grant) = acquire_primary_fallback_grant(deps, &task.id)?
            {
                let fb_ctx =
                    audit_backend::resolve_task_audit_context(deps, None, &task.id, &fb_grant)?;
                if let Ok((fb_provider, fb_reviewer, fb_same)) = fb_ctx {
                    if receipt.head_lock.has_hard_lock() && fb_same {
                        tier1_blocked_fallback = true;
                        let _ = deps.git.discard(&clean);
                    } else {
                        let prev_provider = audit_provider.clone();
                        (audit_provider, reviewer, same_account) =
                            (fb_provider, fb_reviewer, fb_same);
                        pre_mech = cfg
                            .score_gate
                            .then(|| mech_score(deps, &job.worktree.path, &receipt, same_account));
                        deps.emit(
                            EventKind::CrossAudit,
                            &task.id,
                            json!({
                                "phase": "post_gate", "verdict": "auth_fallback",
                                "fallback_from": prev_provider, "provider": audit_provider,
                                "reviewer": reviewer, "same_account": same_account,
                            }),
                        )?;
                        let fb_res = audit_backend::run_auditor_turn(
                            deps,
                            None,
                            &task,
                            &input,
                            &clean.tree,
                            &fb_grant,
                            cfg.score_gate,
                        );
                        let _ = deps.git.discard(&clean);
                        (report, infra, limit_hit) = fb_res?;
                        active_accounts = &*deps.accounts;
                        fallback_grant_slot = Some(fb_grant);
                    }
                } else {
                    let _ = deps.git.discard(&clean);
                }
            } else {
                let _ = deps.git.discard(&clean);
            }
            let active_grant = fallback_grant_slot.as_ref().unwrap_or(grant);
            if let Some(hit) = limit_hit {
                audit_backend::report_audit_limit(
                    deps,
                    active_accounts,
                    &task.id,
                    active_grant,
                    &hit,
                )?;
                let next = audit_backend::route_audit_limit(active_accounts, active_grant, &hit)?;
                return Ok(match next {
                    Next::Switch => Next::SwitchJob(Job::fresh_post_audit(job, receipt)),
                    other => other,
                });
            }
            if let Some(infra) = infra {
                return Ok(if fallback_grant_slot.is_some() || tier1_blocked_fallback {
                    primary_infra_next(deps, &task.id, active_grant, &infra)
                } else {
                    audit_infra_next(deps, &task.id, grant, &infra)
                });
            }
            (
                audit_provider,
                reviewer,
                same_account,
                pre_mech,
                report.verdict,
                report.diff_truncated,
                report.scores,
            )
        };

    let mech =
        pre_mech.unwrap_or_else(|| mech_score(deps, &job.worktree.path, &receipt, same_account));
    let eval = cfg.score_gate.then(|| {
        evaluate_score_gate(
            &mech,
            &scores,
            cfg.min_average_score,
            cfg.min_criterion_score,
        )
    });
    let llm_average = eval.as_ref().and_then(|e| e.llm_average).or_else(|| {
        let n = u32::try_from(GOLDEN_IDS.len()).unwrap_or(7);
        (scores.len() == GOLDEN_IDS.len()).then(|| {
            let sum: u32 = scores.iter().map(|s| u32::from(s.score)).sum();
            u8::try_from(sum / n).unwrap_or(100)
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
    let passed = verdict.is_pass() && !diff_truncated && eval.as_ref().is_none_or(|e| e.passed);
    let score_gate_failed = verdict.is_pass()
        && !diff_truncated
        && eval
            .as_ref()
            .is_some_and(|e| !e.passed && e.llm_average.is_some());
    let event_verdict = if passed {
        "pass".to_string()
    } else if score_gate_failed {
        format!("fail: {}", reasons.join("; "))
    } else {
        audit_backend::verdict_text(&verdict)
    };
    let tree_fp = receipt.tree_fp.as_str();
    let audited = (&verdict, diff_truncated);
    let retry =
        !passed && audit_backend::retry_inconclusive(deps, &task, audited, "tree_fp", tree_fp)?;

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
            "scores": scores,
            "reasons": reasons,
            "tree_fp": receipt.tree_fp,
            "config_hash": receipt.config_hash,
            "retry": retry,
        }),
    )?;
    if retry {
        return Ok(Next::RetryAudit(Job::fresh_post_audit(job, receipt)));
    }

    if passed {
        let canon =
            std::fs::canonicalize(&job.worktree.path).unwrap_or_else(|_| job.worktree.path.clone());
        let record = PostGateAuditRecord {
            repo_root: canon.to_string_lossy().into_owned(),
            tree_fp: receipt.tree_fp.clone(),
            config_hash: receipt.config_hash.clone(),
            verdict: "PASS".to_string(),
            reviewer,
            same_account,
            mechanical_score: mech.score,
            llm_average,
            effective_score,
            scores,
            adjudicated: false,
            dual_consensus: false,
            reviewers: Vec::new(),
            created_at: deps.now(),
        };
        let io = |err: aaf_fp::FpError| PortError::Io(err.to_string());
        let key = aaf_fp::load_or_create_key(&deps.settings.data_dir).map_err(io)?;
        aaf_fp::write_audit_verdict(&deps.settings.data_dir, &key, &job.worktree.path, &record)
            .map_err(io)?;
        return Ok(Next::Gate(GateJob {
            audited: true,
            cached_receipt: Some(Box::new(receipt)),
            ..job
        }));
    }

    let explicit_fail =
        !diff_truncated && (matches!(verdict, AuditVerdict::Fail { .. }) || score_gate_failed);
    if explicit_fail {
        let mut findings =
            collect_rework_findings(&verdict, &scores, &reasons, effective_score, cfg);
        if cfg.score_gate && mech.score < cfg.min_average_score {
            findings.extend(collect_mechanical_findings(
                &receipt,
                &mech,
                cfg.min_average_score,
            ));
        }
        if let Some(next) = unblock::handle_audit_failure(deps, &task, &findings)? {
            return Ok(next);
        }
    }
    let summary = rework_filter::fallback_verdict_summary(&verdict, &event_verdict);
    reject_post_audit(deps, task.id, job.worktree, &summary)
}
