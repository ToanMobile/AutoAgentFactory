//! Pure quorum validation, governing-reviewer score projection, sibling outcome
//! precedence, and 6-branch deterministic Dual-Review aggregation matrix
//! (`docs/PLAN_DUAL_FRONTIER.md` §D.4.3–§D.4.5).

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use aaf_domain::config::CrossAuditConfig;
use aaf_domain::gate::PostGateAuditRecord;
use aaf_domain::pipeline::{
    IssueSeverity, ReviewIssue, ReviewerVerdictRecord, canonical_provider,
    distinct_real_canonical_providers, distinct_real_reviewer_providers, is_known_real_reviewer,
    validate_reviewers,
};
use aaf_fp::PartialAuditRecord;

use super::super::rework_filter;
use super::super::stage_router::ProviderKey;
use crate::cross_audit::{Finding, MechanicalScoreReport, evaluate_score_gate};

/// Non-partial outcome from a sibling `DualReviewTurn`, deferred until all
/// in-flight siblings drain (`in_flight == 0`) with strict precedence (R3-PROTO-3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeferredSiblingOutcome {
    Halt(String),
    Blocked,
    RetryAfter {
        until: i64,
        attempt: u32,
        detail: String,
        provider: ProviderKey,
    },
    Wait {
        until: Option<i64>,
        provider: ProviderKey,
    },
    Switch {
        provider: ProviderKey,
    },
}

/// Deterministic 6-branch aggregation result of Blind Parallel Dual-Review (§D.4.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DualMatrixDecision {
    ConsensusPass {
        record: Box<PostGateAuditRecord>,
    },
    Rework {
        findings: Vec<ReviewIssue>,
        summary: String,
    },
    NeedsAdjudication {
        pass_reviewer_id: String,
        findings: Vec<ReviewIssue>,
        partials: Vec<PartialAuditRecord>,
    },
    Inconclusive {
        reason: String,
    },
}

/// Validates Dual-Review quorum requirements (R3-SEC-5 & R3-PROTO-2).
pub fn validate_dual_quorum(cfg: &CrossAuditConfig, run_executor: &str) -> Result<(), String> {
    validate_reviewers(&cfg.reviewers)?;
    if cfg.reviewers.len() < 2 {
        return Err(format!(
            "dual_review requires at least 2 reviewers in [[cross_audit.reviewers]], found {}",
            cfg.reviewers.len()
        ));
    }
    if cfg.require_different_provider {
        let distinct = distinct_real_reviewer_providers(&cfg.reviewers);
        if distinct < 2 {
            return Err(format!(
                "cross_audit.require_different_provider is true, but [[cross_audit.reviewers]] \
                 provides only {distinct} distinct known real canonical provider(s) (need >= 2; \
                 \"custom\" and executor_cmd overrides are excluded)"
            ));
        }
        let run_canon = canonical_provider(run_executor);
        let has_different = cfg
            .reviewers
            .iter()
            .any(|r| is_known_real_reviewer(r) && canonical_provider(&r.executor) != run_canon);
        if !has_different {
            return Err(format!(
                "cross_audit.require_different_provider is true, but no real reviewer differs \
                 from run executor {run_executor:?}"
            ));
        }
    }
    Ok(())
}

/// Selects the governing reviewer for top-level score projection (§D.4.4):
/// lowest `llm_average` (fail-closed `None -> 0`), tie-breaking by lowest
/// minimum criterion score, then lexicographical `id`.
pub fn governing_reviewer(reviewers: &[ReviewerVerdictRecord]) -> Option<&ReviewerVerdictRecord> {
    reviewers.iter().min_by_key(|r| {
        (
            r.llm_average.unwrap_or(0),
            r.scores.iter().map(|s| s.score).min().unwrap_or(0),
            r.id.as_str(),
        )
    })
}

/// Builds a consensus [`PostGateAuditRecord`] projecting governing-reviewer
/// scores onto top-level fields so `audit_satisfies_policy` never loops (§D.4.4).
pub fn build_dual_audit_record(
    repo_root: &str,
    tree_fp: &str,
    config_hash: &str,
    mechanical_score: u8,
    reviewers: Vec<ReviewerVerdictRecord>,
    adjudicated: bool,
    now: i64,
) -> PostGateAuditRecord {
    let reviewer = reviewers
        .iter()
        .map(|r| format!("{}({})", r.id, r.executor))
        .collect::<Vec<_>>()
        .join("+");
    let (scores, llm_average) = governing_reviewer(&reviewers)
        .map_or((Vec::new(), None), |g| (g.scores.clone(), g.llm_average));
    let effective_score = llm_average.map_or(mechanical_score, |a| mechanical_score.min(a));
    PostGateAuditRecord {
        repo_root: repo_root.to_string(),
        tree_fp: tree_fp.to_string(),
        config_hash: config_hash.to_string(),
        verdict: "PASS".to_string(),
        reviewer,
        same_account: false,
        mechanical_score,
        llm_average,
        effective_score,
        scores,
        dual_consensus: true,
        adjudicated,
        reviewers,
        created_at: now,
    }
}

fn mech_report(score: u8) -> MechanicalScoreReport {
    MechanicalScoreReport {
        score,
        reasons: Vec::new(),
        same_account_warning: false,
    }
}

/// Verifies that a signed [`PostGateAuditRecord`] satisfies Dual-Review policy
/// across every participating reviewer independently (§D.4.4).
pub fn dual_audit_satisfies_policy(record: &PostGateAuditRecord, cfg: &CrossAuditConfig) -> bool {
    if record.verdict != "PASS" || !record.dual_consensus || record.reviewers.len() < 2 {
        return false;
    }
    let mut ids = BTreeSet::new();
    let all_pass = record.reviewers.iter().all(|r| {
        !r.id.is_empty()
            && ids.insert(r.id.as_str())
            && r.verdict == "PASS"
            && r.p0 == 0
            && r.p1 == 0
    });
    if !all_pass {
        return false;
    }
    if cfg.require_different_provider
        && distinct_real_canonical_providers(record.reviewers.iter().map(|r| r.executor.as_str()))
            < 2
    {
        return false;
    }
    if cfg.score_gate {
        if record.effective_score < cfg.min_average_score {
            return false;
        }
        let mech = mech_report(record.mechanical_score);
        return record.reviewers.iter().all(|r| {
            let eval = evaluate_score_gate(
                &mech,
                &r.scores,
                cfg.min_average_score,
                cfg.min_criterion_score,
            );
            eval.passed && r.llm_average == eval.llm_average
        });
    }
    true
}

/// Merges two sibling deferred outcomes enforcing strict precedence:
/// `Halt > Blocked > RetryAfter > Wait > Switch` (R3-CONC-1 & R3-PROTO-3).
pub fn merge_deferred_outcome(
    current: Option<DeferredSiblingOutcome>,
    incoming: DeferredSiblingOutcome,
) -> DeferredSiblingOutcome {
    let rank = |o: &DeferredSiblingOutcome| match o {
        DeferredSiblingOutcome::Halt(_) => 5u8,
        DeferredSiblingOutcome::Blocked => 4,
        DeferredSiblingOutcome::RetryAfter { .. } => 3,
        DeferredSiblingOutcome::Wait { .. } => 2,
        DeferredSiblingOutcome::Switch { .. } => 1,
    };
    let Some(curr) = current else {
        return incoming;
    };
    match rank(&incoming).cmp(&rank(&curr)) {
        Ordering::Greater => incoming,
        Ordering::Less => curr,
        Ordering::Equal => match (curr, incoming) {
            (
                DeferredSiblingOutcome::RetryAfter {
                    until: u1,
                    attempt: a1,
                    detail: d1,
                    provider: p1,
                },
                DeferredSiblingOutcome::RetryAfter {
                    until: u2,
                    attempt: a2,
                    detail: d2,
                    provider: p2,
                },
            ) => {
                if u2 > u1 {
                    DeferredSiblingOutcome::RetryAfter {
                        until: u2,
                        attempt: a1.max(a2),
                        detail: d2,
                        provider: p2,
                    }
                } else {
                    DeferredSiblingOutcome::RetryAfter {
                        until: u1,
                        attempt: a1.max(a2),
                        detail: d1,
                        provider: p1,
                    }
                }
            }
            (
                DeferredSiblingOutcome::Wait { until: u1, .. },
                DeferredSiblingOutcome::Wait {
                    until: Some(t2),
                    provider: p2,
                },
            ) if u1.is_none_or(|t1| t2 > t1) => DeferredSiblingOutcome::Wait {
                until: Some(t2),
                provider: p2,
            },
            (curr, _) => curr,
        },
    }
}

pub(super) fn is_inconclusive_partial(p: &PartialAuditRecord) -> bool {
    !p.verdict.eq_ignore_ascii_case("PASS") && !p.verdict.eq_ignore_ascii_case("FAIL")
}

fn has_blocking_p01(p: &PartialAuditRecord) -> bool {
    p.p0 > 0
        || p.p1 > 0
        || p.issues
            .iter()
            .any(|i| matches!(i.severity, IssueSeverity::P0 | IssueSeverity::P1))
}

fn dedup_blocking_issues(partials: &[&PartialAuditRecord]) -> Vec<ReviewIssue> {
    let mut map: BTreeMap<(String, String), ReviewIssue> = BTreeMap::new();
    for p in partials {
        for i in p
            .issues
            .iter()
            .filter(|i| matches!(i.severity, IssueSeverity::P0 | IssueSeverity::P1))
        {
            let key = (
                i.category.trim().to_ascii_uppercase(),
                i.location.trim().to_string(),
            );
            match map.get_mut(&key) {
                Some(prev) if i.severity < prev.severity => *prev = i.clone(),
                Some(_) => {}
                None => {
                    map.insert(key, i.clone());
                }
            }
        }
    }
    let mut out: Vec<ReviewIssue> = map.into_values().collect();
    out.sort_by(|a, b| {
        (a.severity, &a.category, &a.location, &a.id).cmp(&(
            b.severity,
            &b.category,
            &b.location,
            &b.id,
        ))
    });
    if out.is_empty() {
        for p in partials.iter().filter(|p| p.p0 > 0 || p.p1 > 0) {
            out.push(ReviewIssue {
                id: format!("{}-blocking", p.reviewer_id),
                severity: if p.p0 > 0 {
                    IssueSeverity::P0
                } else {
                    IssueSeverity::P1
                },
                category: "DUAL-REVIEW".to_string(),
                location: String::new(),
                description: format!(
                    "reviewer {} reported {} P0 / {} P1",
                    p.reviewer_id, p.p0, p.p1
                ),
                proposed_fix: String::new(),
            });
        }
    }
    out
}

pub(super) fn collect_score_issues(
    partials: &[&PartialAuditRecord],
    cfg: &CrossAuditConfig,
    mech_score: u8,
) -> Vec<ReviewIssue> {
    let mech = mech_report(mech_score);
    let mut out = Vec::new();
    for p in partials {
        let eval = evaluate_score_gate(
            &mech,
            &p.scores,
            cfg.min_average_score,
            cfg.min_criterion_score,
        );
        if eval.passed {
            continue;
        }
        let low_avg = eval.effective_score < cfg.min_average_score;
        let before = out.len();
        for s in &p.scores {
            if s.score < cfg.min_criterion_score
                || (low_avg && s.score < 100 && !s.deduction_reason.trim().is_empty())
            {
                let reason = if s.deduction_reason.trim().is_empty() {
                    format!("score {}/100 < min {}", s.score, cfg.min_criterion_score)
                } else {
                    s.deduction_reason.trim().to_string()
                };
                out.push(ReviewIssue {
                    id: s.id.clone(),
                    severity: IssueSeverity::P1,
                    category: s.id.clone(),
                    location: reason.clone(),
                    description: format!(
                        "[{}] {} scored {}/100: {reason}",
                        p.reviewer_id, s.id, s.score
                    ),
                    proposed_fix: String::new(),
                });
            }
        }
        if out.len() == before {
            for reason in eval.reasons {
                out.push(ReviewIssue {
                    id: "score_gate".to_string(),
                    severity: IssueSeverity::P1,
                    category: "score_gate".to_string(),
                    location: reason.clone(),
                    description: format!("[{}] {reason}", p.reviewer_id),
                    proposed_fix: String::new(),
                });
            }
        }
    }
    out
}

pub(super) fn to_verdict_rec(
    p: &PartialAuditRecord,
    adjudicated_pass: bool,
) -> ReviewerVerdictRecord {
    ReviewerVerdictRecord {
        id: p.reviewer_id.clone(),
        executor: p.executor.clone(),
        canonical_provider: p.canonical_provider.clone(),
        model: p.model.clone(),
        effort: p.effort.clone(),
        verdict: if adjudicated_pass {
            "PASS".to_string()
        } else {
            p.verdict.clone()
        },
        p0: if adjudicated_pass { 0 } else { p.p0 },
        p1: if adjudicated_pass { 0 } else { p.p1 },
        p2: p.p2,
        llm_average: p.llm_average,
        scores: p.scores.clone(),
    }
}

pub(super) fn summarize_issues(issues: &[ReviewIssue], fallback: &str) -> String {
    if issues.is_empty() {
        return fallback.to_string();
    }
    issues
        .iter()
        .take(5)
        .map(|i| {
            if i.location.trim().is_empty() {
                format!("[{}] {}", i.severity.as_str(), i.category)
            } else {
                format!(
                    "[{}] {} ({})",
                    i.severity.as_str(),
                    i.category,
                    i.location.trim()
                )
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Evaluates all 6 deterministic branches of the Dual-Review matrix (§D.4.5).
pub fn decide_dual_matrix(
    partials: &[PartialAuditRecord],
    cfg: &CrossAuditConfig,
    repo_root: &str,
    tree_fp: &str,
    config_hash: &str,
    mechanical_score: u8,
    now: i64,
) -> DualMatrixDecision {
    let mech = mech_report(mechanical_score);
    let score_ok = |p: &PartialAuditRecord| {
        !cfg.score_gate
            || evaluate_score_gate(
                &mech,
                &p.scores,
                cfg.min_average_score,
                cfg.min_criterion_score,
            )
            .passed
    };
    let (mut inc, mut p01, mut pass, mut score_fail) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for p in partials {
        if is_inconclusive_partial(p) {
            inc.push(p);
        } else if has_blocking_p01(p) {
            p01.push(p);
            if !score_ok(p) {
                score_fail.push(p);
            }
        } else if p.verdict.eq_ignore_ascii_case("PASS") && score_ok(p) {
            pass.push(p);
        } else {
            score_fail.push(p);
        }
    }
    // Branch 6: >= 1 Inconclusive (or < 2 reviewers). Fail-fast if another side has P0/P1.
    if !inc.is_empty() || partials.len() < 2 {
        if !p01.is_empty() {
            let mut findings = dedup_blocking_issues(&p01);
            findings.extend(collect_score_issues(&score_fail, cfg, mechanical_score));
            let summary = summarize_issues(&findings, "dual-review blocking finding");
            return DualMatrixDecision::Rework { findings, summary };
        }
        let reason = if inc.is_empty() {
            format!(
                "dual review requires >= 2 reviewers, got {}",
                partials.len()
            )
        } else {
            inc.iter()
                .map(|p| format!("{}: {}", p.reviewer_id, p.verdict))
                .collect::<Vec<_>>()
                .join("; ")
        };
        return DualMatrixDecision::Inconclusive { reason };
    }
    // Branch 1: all reviewers clean PASS + quorum diversity.
    if pass.len() == partials.len() {
        if cfg.require_different_provider
            && distinct_real_canonical_providers(partials.iter().map(|p| p.executor.as_str())) < 2
        {
            return DualMatrixDecision::Inconclusive {
                reason: "dual review consensus requires >= 2 distinct real canonical providers"
                    .to_string(),
            };
        }
        let recs = partials.iter().map(|p| to_verdict_rec(p, false)).collect();
        let rec = build_dual_audit_record(
            repo_root,
            tree_fp,
            config_hash,
            mechanical_score,
            recs,
            false,
            now,
        );
        return DualMatrixDecision::ConsensusPass {
            record: Box::new(rec),
        };
    }
    // Branch 3: 0 P0/P1, >= 1 failed score_gate.
    if p01.is_empty() {
        let findings = collect_score_issues(&score_fail, cfg, mechanical_score);
        let summary = summarize_issues(&findings, "dual-review score gate failed");
        return DualMatrixDecision::Rework { findings, summary };
    }
    // Branch 4 (cross_adjudicate = true) & Branch 5 (cross_adjudicate = false).
    if p01.len() == 1 && score_fail.is_empty() && !pass.is_empty() {
        let findings = dedup_blocking_issues(&p01);
        if cfg.cross_adjudicate {
            return DualMatrixDecision::NeedsAdjudication {
                pass_reviewer_id: pass[0].reviewer_id.clone(),
                findings,
                partials: partials.to_vec(),
            };
        }
        let summary = summarize_issues(&findings, "dual-review finding (adjudication disabled)");
        return DualMatrixDecision::Rework { findings, summary };
    }
    // Branch 2: multiple reviewers failed.
    let mut findings = dedup_blocking_issues(&p01);
    findings.extend(collect_score_issues(&score_fail, cfg, mechanical_score));
    let summary = summarize_issues(&findings, "dual-review failed by multiple reviewers");
    DualMatrixDecision::Rework { findings, summary }
}

pub(super) fn issues_to_findings(issues: &[ReviewIssue]) -> Vec<Finding> {
    issues
        .iter()
        .map(|i| {
            let id = if rework_filter::is_score_finding(&i.id) || i.category.trim().is_empty() {
                i.id.clone()
            } else {
                i.category.clone()
            };
            let fix = i.proposed_fix.trim();
            let question = if fix.is_empty() {
                format!("[{}] {}", i.severity.as_str(), i.description.trim())
            } else {
                format!(
                    "[{}] {} (fix: {fix})",
                    i.severity.as_str(),
                    i.description.trim()
                )
            };
            Finding {
                id,
                question,
                evidence: i.location.trim().to_string(),
            }
        })
        .collect()
}
