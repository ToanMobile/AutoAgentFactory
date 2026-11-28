//! Blind reviewer turn (`run_reviewer_turn`) and Cross-Adjudication turn
//! (`run_adjudicate_turn`) execution helpers for [`super`].

use aaf_domain::budget::BudgetCheck;
use aaf_domain::event::EventKind;
use aaf_domain::gate::Receipt;
use aaf_domain::pipeline::{IssueSeverity, ReviewIssue, ReviewerConfig, canonical_provider};
use aaf_domain::ports::{AgentOutcome, InfraFailure, InfraKind, LimitHit, PortResult};
use aaf_domain::run::RunStatus;
use aaf_domain::task::Task;
use aaf_fp::PartialAuditRecord;
use serde_json::json;

use super::matrix::{collect_score_issues, summarize_issues, to_verdict_rec};
use super::{
    PartialTurnResult, build_dual_audit_record, canon_root, dual_audit_satisfies_policy,
    dual_mech_score, dual_mech_score_with, emit_fail_and_rework, fp_io, sign_and_emit_pass,
};
use crate::cross_audit::AUDIT_EXECUTOR;
use crate::cross_audit::dual_verdict::{
    DualReviewerOutcome, adjudicate_json_schema, build_adjudicate_prompt_with_limit,
    build_dual_checklist, build_dual_review_prompt_with_limit, dual_review_json_schema,
    evaluate_adjudication, names_dual_blocker, parse_and_validate_dual_review_with_limit,
};
use crate::cross_audit::prompt::{
    PRIOR_BEGIN, PRIOR_END, clip_repair_prior, neutralise, summarize_diff_for_audit,
};
use crate::factory::audit_backend::{report_audit_limit, route_audit_limit};
use crate::factory::audit_turn::short_circuit_audit_reason_with_limit;
use crate::factory::plan_job;
use crate::factory::ports::{Grant, IntegrationGuard};
use crate::factory::post_audit;
use crate::factory::stage_router::StageTarget;
use crate::factory::worker::{self, GateJob, Next};
use crate::factory::{Deps, unblock};
use crate::git::TaskWorktree;
use crate::unblocker::QuotaRoute;

const AUTH_QUARANTINE_SECS: i64 = 3600;
const DUAL_REVIEW_MAX_TURNS: u32 = 16;
const DUAL_REPAIR_MAX_TURNS: u32 = 8;
const DUAL_ADJUDICATE_MAX_TURNS: u32 = 12;
const DUAL_REPAIR_PRIOR_CHARS: usize = 12_000;
const BEHAVIORAL_RED_NOTE: &str = "Oracle behavioral_red_unproven: verify that the new module's \
     tests contain non-vacuous behavioral assertions exercising real production logic and edge cases";

fn with_clean_tree<T>(
    deps: &Deps,
    f: impl FnOnce(&TaskWorktree) -> PortResult<T>,
) -> PortResult<T> {
    let clean = IntegrationGuard::acquire(&*deps.git, &[])?;
    f(&clean.integration().tree)
}

#[allow(clippy::too_many_arguments)]
fn run_clean_turn(
    deps: &Deps,
    target: &StageTarget,
    task: &Task,
    clean_tree: &TaskWorktree,
    prompt: String,
    schema: String,
    max_turns: u32,
    grant: &Grant,
) -> PortResult<Result<(String, AgentOutcome), Next>> {
    let mut run = worker::begin_run_with(
        deps,
        task,
        clean_tree,
        AUDIT_EXECUTOR,
        target.provider_kind,
        grant,
        target.hook_required,
        &*target.accounts,
    )?;
    run.request.sandbox.read_only = true;
    run.request.prompt = prompt;
    run.request.json_schema = (!schema.trim().is_empty()).then_some(schema);
    run.request.max_turns = Some(max_turns);
    let out = match target.runner.run(&run.request) {
        Ok(o) => o,
        Err(err) => {
            worker::end_run(
                deps,
                task,
                &run,
                RunStatus::Failed,
                None,
                (0, 0),
                false,
                false,
            )?;
            return Err(err);
        }
    };
    let tok = (out.tokens_in, out.tokens_out);
    let measured = target.runner.reports_tokens();
    let charge_wall = worker::should_charge_wall(&out);
    worker::end_run(
        deps,
        task,
        &run,
        worker::status_of(&out),
        out.exit_code,
        tok,
        measured,
        charge_wall,
    )?;
    if let Some(hit) = out.limit_hit {
        report_audit_limit(deps, &*target.accounts, &task.id, grant, &hit)?;
        return Ok(Err(route_audit_limit(&*target.accounts, grant, &hit)?));
    }
    if let Some(infra) = out.infra {
        return Ok(Err(dual_infra_next(deps, target, &task.id, grant, &infra)?));
    }
    Ok(Ok((run.request.run_id, out)))
}

fn dual_infra_next(
    deps: &Deps,
    target: &StageTarget,
    task_id: &str,
    grant: &Grant,
    infra: &InfraFailure,
) -> PortResult<Next> {
    let (what, fix) = worker::infra_words(grant, infra);
    let detail = worker::halt_message(task_id, what, infra, &fix);
    if matches!(infra.kind, InfraKind::Network | InfraKind::OAuthRefreshed) {
        return Ok(Next::RetryAfter {
            until: deps.now(),
            attempt: 1,
            detail,
            oauth_refreshed: infra.kind == InfraKind::OAuthRefreshed,
        });
    }
    if infra.kind == InfraKind::Auth
        && let Some(acct) = &grant.account
        && target.accounts.provider_account_count()? >= 2
    {
        let (now, until) = (deps.now(), deps.now().saturating_add(AUTH_QUARANTINE_SECS));
        let raw = format!("auth: {}", unblock::one_line(&infra.raw, 300));
        let hit = LimitHit {
            reset_at: Some(until),
            raw: raw.clone(),
        };
        target.accounts.report_limit(&acct.id, &hit)?;
        let qk = |id: &str| format!("account_auth_quarantine:{id}");
        deps.store.set_meta(&qk(&acct.id), &until.to_string())?;
        deps.emit(
            EventKind::AccountCooldown,
            task_id,
            json!({
                "account": acct.id, "reset_at": Some(until),
                "reason": "auth_quarantine", "raw": raw,
            }),
        )?;
        let can_rotate = match target.accounts.after_limit()? {
            Some(QuotaRoute::Switch) => true,
            Some(QuotaRoute::Wait) => false,
            None => target.accounts.profiles()?.iter().any(|p| {
                p.provider == acct.provider
                    && p.id != acct.id
                    && deps
                        .store
                        .get_meta(&qk(&p.id))
                        .ok()
                        .flatten()
                        .and_then(|v| v.parse::<i64>().ok())
                        .is_none_or(|exp| exp <= now)
            }),
        };
        if can_rotate {
            return Ok(Next::Switch);
        }
    }
    Ok(Next::Halt(detail))
}

fn make_partial_record(
    job: &GateJob,
    receipt: &Receipt,
    plan_hash: &str,
    rev: &ReviewerConfig,
    mech: u8,
    outcome: DualReviewerOutcome,
    now: i64,
) -> (PartialAuditRecord, Vec<String>, bool) {
    let (verdict, scores, llm_average, issues, score_fails, persist) = match outcome {
        DualReviewerOutcome::Pass {
            scores,
            llm_average,
            p2_issues,
        } => (
            "PASS".to_string(),
            scores,
            llm_average,
            p2_issues,
            Vec::new(),
            true,
        ),
        DualReviewerOutcome::Fail {
            issues,
            scores,
            llm_average,
            score_fail_reasons,
        } => (
            "FAIL".to_string(),
            scores,
            llm_average,
            issues,
            score_fail_reasons,
            true,
        ),
        DualReviewerOutcome::Inconclusive { reason } => (
            format!("INCONCLUSIVE: {}", unblock::one_line(&reason, 200)),
            Vec::new(),
            None,
            Vec::new(),
            Vec::new(),
            false,
        ),
    };
    let cnt = |sev| {
        u32::try_from(issues.iter().filter(|i| i.severity == sev).count()).unwrap_or(u32::MAX)
    };
    let rec = PartialAuditRecord {
        repo_root: canon_root(job),
        tree_fp: receipt.tree_fp.clone(),
        base_sha: receipt.base_sha.clone().unwrap_or_default(),
        config_hash: receipt.config_hash.clone(),
        plan_contract_hash: plan_hash.to_string(),
        reviewer_id: rev.id.clone(),
        executor: rev.executor.clone(),
        canonical_provider: canonical_provider(&rev.executor),
        model: rev.model.clone(),
        effort: rev.effort.clone(),
        verdict,
        p0: cnt(IssueSeverity::P0),
        p1: cnt(IssueSeverity::P1),
        p2: cnt(IssueSeverity::P2),
        mechanical_score: mech,
        llm_average,
        effective_score: llm_average.map_or(mech, |a| mech.min(a)),
        scores,
        issues,
        created_at: now,
    };
    (rec, score_fails, persist)
}

/// The scoring knobs of one reviewer turn, taken from `[cross_audit]`.
#[derive(Debug, Clone)]
pub(crate) struct ReviewPolicy {
    pub score_gate: bool,
    pub min_average_score: u8,
    pub min_criterion_score: u8,
    pub max_diff_bytes: usize,
}

impl ReviewPolicy {
    pub(crate) fn from_cross_audit(cfg: &aaf_domain::config::CrossAuditConfig) -> Self {
        Self {
            score_gate: cfg.score_gate,
            min_average_score: cfg.min_average_score,
            min_criterion_score: cfg.min_criterion_score,
            max_diff_bytes: cfg.max_diff_bytes,
        }
    }
}

/// What a reviewer turn is told rather than reads from the factory's state.
#[derive(Debug, Clone)]
pub(crate) struct ReviewInputs {
    pub policy: ReviewPolicy,
    /// `None`: review the task branch's own diff (`GitOps::diff`). `Some(sha)`:
    /// review `sha...HEAD` of the worktree (`GitOps::diff_from`), the PR's own
    /// change however far the base has moved.
    pub diff_base: Option<String>,
    /// `None`: ask [`post_audit::proof_verified`] as the factory does. `Some`:
    /// the caller already verified the proof and says so (see
    /// [`dual_mech_score_with`]).
    pub proof_ok: Option<bool>,
}

impl ReviewInputs {
    /// What the factory's own dual review has always used.
    pub(crate) fn current(deps: &Deps) -> Self {
        Self {
            policy: ReviewPolicy::from_cross_audit(&deps.settings.repo.cross_audit),
            diff_base: None,
            proof_ok: None,
        }
    }
}

/// Runs one blind reviewer turn in a clean base tree with 1 format-repair retry,
/// signs `audit_partial\0` on PASS/FAIL, and emits `EventKind::DualReviewTurn`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_reviewer_turn(
    deps: &Deps,
    target: &StageTarget,
    reviewer: &ReviewerConfig,
    job: &GateJob,
    receipt: &Receipt,
    plan_hash: &str,
    grant: &Grant,
    inputs: &ReviewInputs,
) -> PortResult<PartialTurnResult> {
    let task = deps.store.get_task(&job.task_id)?;
    let cfg = &inputs.policy;
    let mech = dual_mech_score_with(deps, job, receipt, inputs.proof_ok);
    let raw_diff = match inputs.diff_base.as_deref() {
        None => deps.git.diff(&job.worktree)?,
        Some(base) => deps.git.diff_from(&job.worktree, base)?,
    };
    if let Some(reason) = short_circuit_audit_reason_with_limit(&raw_diff, cfg.max_diff_bytes) {
        let out = DualReviewerOutcome::Inconclusive { reason };
        let (rec, _, _) =
            make_partial_record(job, receipt, plan_hash, reviewer, mech, out, deps.now());
        deps.emit(
            EventKind::DualReviewTurn,
            &task.id,
            json!({
                "reviewer_id": reviewer.id, "executor": reviewer.executor, "verdict": rec.verdict,
                "short_circuited": true, "tree_fp": receipt.tree_fp,
                "config_hash": receipt.config_hash,
            }),
        )?;
        return Ok(PartialTurnResult::Completed(Box::new(rec)));
    }
    let diff = summarize_diff_for_audit(&raw_diff);
    let plan = plan_job::latest_converged_plan(deps, &task.id)?.map(|(p, _)| p);
    let mut spec = task.spec.clone();
    if post_audit::has_behavioral_red_unproven(receipt) {
        spec.acceptance.push(BEHAVIORAL_RED_NOTE.to_string());
    }
    let checklist = build_dual_checklist(&spec, deps.settings.mobile, plan.is_some());
    let base_prompt = build_dual_review_prompt_with_limit(
        &task,
        &diff,
        plan.as_ref(),
        &checklist,
        cfg.score_gate,
        cfg.max_diff_bytes,
    );
    let schema = dual_review_json_schema(cfg.score_gate);
    let (parsed, last_run_id) = match with_clean_tree(deps, |tree| {
        let (mut prompt, mut run_id) = (base_prompt.clone(), String::new());
        let mut active_schema = schema.clone();
        let mut max_turns = DUAL_REVIEW_MAX_TURNS;
        let mut res = DualReviewerOutcome::Inconclusive {
            reason: "reviewer did not run".to_string(),
        };
        let (mut saw_named_blocker, mut prior_inconclusive) = (false, String::new());
        for attempt in 0..2u8 {
            let (rid, out) = match run_clean_turn(
                deps,
                target,
                &task,
                tree,
                prompt,
                active_schema.clone(),
                max_turns,
                grant,
            )? {
                Ok(pair) => pair,
                Err(next) => return Ok(Err(next)),
            };
            run_id = rid;
            if !out.succeeded() {
                res = DualReviewerOutcome::Inconclusive {
                    reason: format!(
                        "reviewer failed (code {:?}, timed_out={})",
                        out.exit_code, out.timed_out
                    ),
                };
                break;
            }
            res = parse_and_validate_dual_review_with_limit(
                &out.final_text,
                &checklist,
                &diff,
                plan.as_ref(),
                cfg.score_gate,
                cfg.min_average_score,
                cfg.min_criterion_score,
                mech,
                cfg.max_diff_bytes,
            );
            if attempt == 1 && saw_named_blocker && matches!(res, DualReviewerOutcome::Pass { .. })
            {
                res = DualReviewerOutcome::Inconclusive {
                    reason: format!(
                        "repair turn attempted to downgrade prior blocking finding to PASS ({prior_inconclusive})"
                    ),
                };
                break;
            }
            if let DualReviewerOutcome::Inconclusive { reason } = &res
                && attempt == 0
                && worker::ensure_budget(deps, &task.root_id)?.check() == BudgetCheck::Ok
            {
                let prior_empty = out.final_text.trim().is_empty();
                if prior_empty {
                    active_schema.clear();
                    max_turns = DUAL_REVIEW_MAX_TURNS;
                } else {
                    max_turns = DUAL_REPAIR_MAX_TURNS;
                }
                if names_dual_blocker(&out.final_text, &checklist)
                    || reason.contains("citation verification")
                {
                    saw_named_blocker = true;
                    prior_inconclusive = reason.clone();
                }
                let clipped = clip_repair_prior(&out.final_text, DUAL_REPAIR_PRIOR_CHARS);
                prompt = format!(
                    "{base_prompt}\n\nYour previous reply failed validation. Fix the JSON/citation \
                     error directly using the supplied DIFF without re-scanning the repository, \
                     and reply with ONE strict JSON object matching the schema.\n{PRIOR_BEGIN}\n\
                     Parse error: {}\n{}\n{PRIOR_END}\n",
                    neutralise(reason),
                    neutralise(&clipped)
                );
            } else {
                break;
            }
        }
        Ok(Ok((res, run_id)))
    })? {
        Ok(pair) => pair,
        Err(next) => return Ok(PartialTurnResult::Deferred(next)),
    };
    let (rec, score_fails, persist) =
        make_partial_record(job, receipt, plan_hash, reviewer, mech, parsed, deps.now());
    if persist {
        let key = aaf_fp::load_or_create_key(&deps.settings.data_dir).map_err(fp_io)?;
        aaf_fp::write_partial_audit(&deps.settings.data_dir, &key, &job.worktree.path, &rec)
            .map_err(fp_io)?;
    }
    deps.emit(
        EventKind::DualReviewTurn,
        &task.id,
        json!({
            "reviewer_id": rec.reviewer_id, "executor": rec.executor,
            "canonical_provider": rec.canonical_provider, "model": rec.model,
            "effort": rec.effort, "verdict": rec.verdict, "p0": rec.p0, "p1": rec.p1, "p2": rec.p2,
            "mechanical_score": rec.mechanical_score, "llm_average": rec.llm_average,
            "effective_score": rec.effective_score, "score_fail_reasons": score_fails,
            "tree_fp": receipt.tree_fp, "config_hash": receipt.config_hash,
            "plan_contract_hash": plan_hash, "run_id": last_run_id,
        }),
    )?;
    Ok(PartialTurnResult::Completed(Box::new(rec)))
}

/// Runs the single Cross-Adjudication turn (Branch 4) in a clean base tree.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_adjudicate_turn(
    deps: &Deps,
    target: &StageTarget,
    job: &GateJob,
    receipt: &Receipt,
    plan_hash: &str,
    findings: &[ReviewIssue],
    partials: &[PartialAuditRecord],
    grant: &Grant,
) -> PortResult<Next> {
    let task = deps.store.get_task(&job.task_id)?;
    let diff = summarize_diff_for_audit(&deps.git.diff(&job.worktree)?);
    let plan = plan_job::latest_converged_plan(deps, &task.id)?.map(|(p, _)| p);
    let prompt = build_adjudicate_prompt_with_limit(
        &task,
        &diff,
        plan.as_ref(),
        findings,
        deps.settings.repo.cross_audit.max_diff_bytes,
    );
    let schema = adjudicate_json_schema().to_string();
    let (run_id, adj) = match with_clean_tree(deps, |tree| {
        match run_clean_turn(
            deps,
            target,
            &task,
            tree,
            prompt,
            schema,
            DUAL_ADJUDICATE_MAX_TURNS,
            grant,
        )? {
            Ok((rid, out)) => {
                let raw = if out.succeeded() { &out.final_text } else { "" };
                Ok(Ok((
                    rid,
                    evaluate_adjudication(raw, findings, &diff, &tree.path),
                )))
            }
            Err(next) => Ok(Err(next)),
        }
    })? {
        Ok(pair) => pair,
        Err(next) => return Ok(next),
    };
    deps.emit(
        EventKind::DualReviewTurn,
        &task.id,
        json!({
            "phase": "adjudicate", "run_id": run_id, "plan_contract_hash": plan_hash,
            "surviving": adj.surviving_issues.len(), "dropped_p1": adj.dropped_p1_issues.len(),
            "upheld_reasons": adj.upheld_reasons,
        }),
    )?;
    let mech = dual_mech_score(deps, job, receipt);
    if adj.surviving_issues.is_empty() {
        let recs = partials.iter().map(|p| to_verdict_rec(p, true)).collect();
        let rec = build_dual_audit_record(
            &canon_root(job),
            &receipt.tree_fp,
            &receipt.config_hash,
            mech,
            recs,
            true,
            deps.now(),
        );
        let cfg = &deps.settings.repo.cross_audit;
        if dual_audit_satisfies_policy(&rec, cfg) {
            return sign_and_emit_pass(deps, &task.id, job, receipt, &rec);
        }
        let partial_refs: Vec<&PartialAuditRecord> = partials.iter().collect();
        let score_issues = collect_score_issues(&partial_refs, cfg, mech);
        let rework_issues = if score_issues.is_empty() {
            vec![ReviewIssue {
                id: "score_gate".to_string(),
                severity: IssueSeverity::P1,
                category: "score_gate".to_string(),
                location: String::new(),
                description:
                    "adjudicated dual-review record failed score-gate or policy verification"
                        .to_string(),
                proposed_fix: String::new(),
            }]
        } else {
            score_issues
        };
        let summary = format!(
            "dual-review score gate failed after adjudication: {}",
            summarize_issues(&rework_issues, "dual-review score gate failed")
        );
        return emit_fail_and_rework(
            deps,
            &task,
            job,
            receipt,
            mech,
            true,
            &rework_issues,
            &summary,
        );
    }
    let summary = summarize_issues(
        &adj.surviving_issues,
        "adjudication upheld blocking findings",
    );
    emit_fail_and_rework(
        deps,
        &task,
        job,
        receipt,
        mech,
        true,
        &adj.surviving_issues,
        &summary,
    )
}
