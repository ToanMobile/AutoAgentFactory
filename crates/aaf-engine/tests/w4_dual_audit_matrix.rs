#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Wave 4 Dual-Review matrix, quorum validation, governing-reviewer projection,
//! policy verification, and deferred sibling outcome precedence tests.

use aaf_domain::config::CrossAuditConfig;
use aaf_domain::event::EventKind;
use aaf_domain::gate::GoldenScore;
use aaf_domain::pipeline::{
    IssueSeverity, ReviewIssue, ReviewerConfig, ReviewerVerdictRecord, canonical_provider,
};
use aaf_engine::cross_audit::GOLDEN_IDS;
use aaf_engine::factory::dual_audit::{
    DeferredSiblingOutcome, DualMatrixDecision, build_dual_audit_record, decide_dual_matrix,
    dual_audit_satisfies_policy, governing_reviewer, merge_deferred_outcome, validate_dual_quorum,
};
use aaf_engine::factory::stage_router::ProviderKey;
use aaf_engine::handover_dual::check_dual_review_missing;
use aaf_engine::telemetry::summarize_event_one_line;
use aaf_fp::PartialAuditRecord;

fn rev(id: &str, executor: &str, cmd: &str) -> ReviewerConfig {
    ReviewerConfig {
        id: id.into(),
        executor: executor.into(),
        executor_cmd: cmd.into(),
        model: "m".into(),
        effort: "high".into(),
    }
}

fn scores_with(vals: [u8; 7]) -> (Vec<GoldenScore>, u8) {
    let scores = GOLDEN_IDS
        .iter()
        .zip(vals)
        .map(|(id, score)| GoldenScore {
            id: (*id).into(),
            score,
            deduction_reason: if score < 100 {
                format!("{id} deducted to {score}")
            } else {
                String::new()
            },
        })
        .collect();
    let avg = u8::try_from(vals.iter().map(|&v| u32::from(v)).sum::<u32>() / 7).unwrap();
    (scores, avg)
}

fn verdict_rec(id: &str, executor: &str, vals: Option<[u8; 7]>) -> ReviewerVerdictRecord {
    let (scores, llm_average) = vals.map_or((Vec::new(), None), |v| {
        let (s, a) = scores_with(v);
        (s, Some(a))
    });
    ReviewerVerdictRecord {
        id: id.into(),
        executor: executor.into(),
        canonical_provider: canonical_provider(executor),
        model: "m".into(),
        effort: "high".into(),
        verdict: "PASS".into(),
        p0: 0,
        p1: 0,
        p2: 0,
        llm_average,
        scores,
    }
}

fn issue(id: &str, severity: IssueSeverity, cat: &str, loc: &str) -> ReviewIssue {
    ReviewIssue {
        id: id.into(),
        severity,
        category: cat.into(),
        location: loc.into(),
        description: format!("{cat} defect at {loc}"),
        proposed_fix: "fix it".into(),
    }
}

fn partial(
    id: &str,
    exec: &str,
    verdict: &str,
    vals: Option<[u8; 7]>,
    issues: Vec<ReviewIssue>,
) -> PartialAuditRecord {
    let (scores, llm_average) = vals.map_or((Vec::new(), None), |v| {
        let (s, a) = scores_with(v);
        (s, Some(a))
    });
    let cnt = |sev| u32::try_from(issues.iter().filter(|i| i.severity == sev).count()).unwrap();
    PartialAuditRecord {
        repo_root: "/repo".into(),
        tree_fp: "git-tree:abc".into(),
        base_sha: "base123".into(),
        config_hash: "cfg123".into(),
        plan_contract_hash: "plan123".into(),
        reviewer_id: id.into(),
        executor: exec.into(),
        canonical_provider: canonical_provider(exec),
        model: "m".into(),
        effort: "high".into(),
        verdict: verdict.into(),
        p0: cnt(IssueSeverity::P0),
        p1: cnt(IssueSeverity::P1),
        p2: cnt(IssueSeverity::P2),
        mechanical_score: 100,
        llm_average,
        effective_score: llm_average.unwrap_or(100),
        scores,
        issues,
        created_at: 1000,
    }
}

fn dual_cfg(score_gate: bool, cross_adjudicate: bool, diff_provider: bool) -> CrossAuditConfig {
    CrossAuditConfig {
        dual_review: true,
        score_gate,
        cross_adjudicate,
        require_different_provider: diff_provider,
        min_average_score: 95,
        min_criterion_score: 90,
        reviewers: vec![
            rev("agy-rev", "antigravity", ""),
            rev("claude-rev", "claude", ""),
        ],
        ..CrossAuditConfig::default()
    }
}

#[test]
fn validate_dual_quorum_enforces_real_distinct_providers_and_valid_ids() {
    let ok = dual_cfg(true, true, true);
    assert!(validate_dual_quorum(&ok, "claude").is_ok());

    let mut one = ok.clone();
    one.reviewers.pop();
    assert!(validate_dual_quorum(&one, "claude").is_err());

    let mut bad_id = ok.clone();
    bad_id.reviewers[0].id = "../escape".into();
    assert!(validate_dual_quorum(&bad_id, "claude").is_err());

    let mut dup_id = ok.clone();
    dup_id.reviewers[1].id = dup_id.reviewers[0].id.clone();
    assert!(validate_dual_quorum(&dup_id, "claude").is_err());

    // Rejects "custom" and non-empty executor_cmd when require_different_provider = true.
    let mut custom = ok.clone();
    custom.reviewers[0] = rev("custom-rev", "custom", "");
    assert!(validate_dual_quorum(&custom, "claude").is_err());

    let mut wrapped = ok.clone();
    wrapped.reviewers[0] = rev("wrapped-agy", "antigravity", "/usr/local/bin/wrapper");
    assert!(validate_dual_quorum(&wrapped, "claude").is_err());

    // Same canonical family (gemini + antigravity -> 1 canonical provider) fails closed.
    let mut same_fam = ok;
    same_fam.reviewers = vec![
        rev("agy-rev", "antigravity", ""),
        rev("gem-rev", "gemini", ""),
    ];
    assert!(validate_dual_quorum(&same_fam, "claude").is_err());

    same_fam.require_different_provider = false;
    assert!(validate_dual_quorum(&same_fam, "claude").is_ok());
}

#[test]
fn governing_reviewer_projects_lowest_avg_then_min_criterion_then_id() {
    let high = verdict_rec("rev-high", "claude", Some([98; 7]));
    let low = verdict_rec("rev-low", "antigravity", Some([95; 7]));
    assert_eq!(
        governing_reviewer(&[high.clone(), low]).unwrap().id,
        "rev-low"
    );

    // Conservative fail-closed: None (0) beats Some(98).
    let unscored = verdict_rec("rev-none", "antigravity", None);
    assert_eq!(
        governing_reviewer(&[high, unscored]).unwrap().id,
        "rev-none"
    );

    // Tie-break on same llm_average (95): lower min-criterion (90 vs 95) wins.
    let spread = verdict_rec("z-spread", "claude", Some([90, 96, 96, 96, 96, 96, 95]));
    let flat = verdict_rec("a-flat", "antigravity", Some([95; 7]));
    assert_eq!(spread.llm_average, flat.llm_average);
    assert_eq!(
        governing_reviewer(&[flat, spread.clone()]).unwrap().id,
        "z-spread"
    );

    // Tie-break on same llm_average and min-criterion: lexicographical id wins.
    let alpha = verdict_rec("alpha", "antigravity", Some([90, 96, 96, 96, 96, 96, 95]));
    let rec = build_dual_audit_record("/repo", "fp1", "cfg1", 98, vec![spread, alpha], true, 42);
    assert_eq!(rec.reviewer, "z-spread(claude)+alpha(antigravity)");
    assert!(rec.dual_consensus && rec.adjudicated && !rec.same_account);
    assert_eq!(rec.llm_average, Some(95));
    assert_eq!(rec.effective_score, 95);
    assert_eq!(rec.scores[0].score, 90);
}

#[test]
fn dual_audit_satisfies_policy_checks_consensus_providers_and_every_reviewer_score() {
    let cfg = dual_cfg(true, true, true);
    let r1 = verdict_rec("agy", "antigravity", Some([96; 7]));
    let r2 = verdict_rec("claude", "claude", Some([98; 7]));
    let valid = build_dual_audit_record("/repo", "fp", "cfg", 100, vec![r1, r2.clone()], false, 1);
    assert!(dual_audit_satisfies_policy(&valid, &cfg));

    let mut no_cons = valid.clone();
    no_cons.dual_consensus = false;
    assert!(!dual_audit_satisfies_policy(&no_cons, &cfg));

    let c_recs = vec![
        verdict_rec("custom-rev", "custom", Some([98; 7])),
        r2.clone(),
    ];
    let bad_prov = build_dual_audit_record("/repo", "fp", "cfg", 100, c_recs, false, 1);
    assert!(!dual_audit_satisfies_policy(&bad_prov, &cfg));

    // Rejects when 1 of 2 reviewers fails score_gate (criterion 85 < 90).
    let low_crit = verdict_rec("agy-low", "antigravity", Some([85, 99, 99, 99, 99, 99, 99]));
    let one_bad = build_dual_audit_record("/repo", "fp", "cfg", 100, vec![low_crit, r2], false, 1);
    assert!(!dual_audit_satisfies_policy(&one_bad, &cfg));

    let mut with_p1 = valid;
    with_p1.reviewers[0].p1 = 1;
    assert!(!dual_audit_satisfies_policy(&with_p1, &cfg));
}

fn run_matrix(ps: &[PartialAuditRecord], cfg: &CrossAuditConfig) -> DualMatrixDecision {
    decide_dual_matrix(ps, cfg, "/repo", "fp", "cfg", 100, 10)
}

#[test]
fn decide_dual_matrix_covers_all_six_branches() {
    let cfg = dual_cfg(true, true, true);
    let pass_a = partial("agy", "antigravity", "PASS", Some([96; 7]), vec![]);
    let pass_b = partial("claude", "claude", "PASS", Some([98; 7]), vec![]);

    // Branch 1: ConsensusPass.
    match run_matrix(&[pass_a.clone(), pass_b.clone()], &cfg) {
        DualMatrixDecision::ConsensusPass { record } => {
            assert!(record.dual_consensus && !record.adjudicated);
            assert_eq!(record.llm_average, Some(96));
            assert!(dual_audit_satisfies_policy(&record, &cfg));
        }
        other => panic!("expected Branch 1 ConsensusPass, got {other:?}"),
    }

    // Branch 2: Both fail -> merge & dedup keeping P0 over P1 on same (category, location).
    let fail_a = partial(
        "agy",
        "antigravity",
        "FAIL",
        Some([96; 7]),
        vec![
            issue("A1", IssueSeverity::P1, "SECRET", "src/auth.rs:10"),
            issue("A2", IssueSeverity::P1, "PLAN-DRIFT", "src/new.rs:1"),
        ],
    );
    let fail_b = partial(
        "claude",
        "claude",
        "FAIL",
        Some([96; 7]),
        vec![issue("B1", IssueSeverity::P0, "SECRET", "src/auth.rs:10")],
    );
    match run_matrix(&[fail_a.clone(), fail_b], &cfg) {
        DualMatrixDecision::Rework { findings, .. } => {
            assert_eq!(findings.len(), 2);
            assert_eq!(findings[0].severity, IssueSeverity::P0);
            assert_eq!(findings[0].category, "SECRET");
            assert_eq!(findings[1].category, "PLAN-DRIFT");
        }
        other => panic!("expected Branch 2 Rework, got {other:?}"),
    }

    // Branch 3: 0 P0/P1 on both, 1 fails score_gate -> Rework with deduction reasons.
    let low_score = partial(
        "agy",
        "antigravity",
        "FAIL",
        Some([88, 98, 98, 98, 98, 98, 98]),
        vec![],
    );
    match run_matrix(&[low_score, pass_b.clone()], &cfg) {
        DualMatrixDecision::Rework { findings, .. } => {
            assert_eq!(findings.len(), 1);
            assert_eq!(findings[0].category, "G1");
        }
        other => panic!("expected Branch 3 Rework, got {other:?}"),
    }

    // Branch 4: 1 has P0/P1, other is clean PASS, cross_adjudicate = true -> NeedsAdjudication.
    match run_matrix(&[fail_a.clone(), pass_b.clone()], &cfg) {
        DualMatrixDecision::NeedsAdjudication {
            pass_reviewer_id,
            findings,
            partials,
        } => {
            assert_eq!(pass_reviewer_id, "claude");
            assert_eq!(findings.len(), 2);
            assert_eq!(partials.len(), 2);
        }
        other => panic!("expected Branch 4 NeedsAdjudication, got {other:?}"),
    }

    // Branch 5: Same as Branch 4 with cross_adjudicate = false -> fail-closed Rework.
    let cfg_no_adj = dual_cfg(true, false, true);
    match run_matrix(&[fail_a.clone(), pass_b], &cfg_no_adj) {
        DualMatrixDecision::Rework { findings, .. } => assert_eq!(findings.len(), 2),
        other => panic!("expected Branch 5 Rework, got {other:?}"),
    }

    // Branch 6a: Inconclusive + valid P0/P1 -> fail-fast Rework.
    let inc = partial("claude", "claude", "INCONCLUSIVE: timeout", None, vec![]);
    match run_matrix(&[fail_a, inc.clone()], &cfg) {
        DualMatrixDecision::Rework { findings, .. } => assert_eq!(findings.len(), 2),
        other => panic!("expected Branch 6a fail-fast Rework, got {other:?}"),
    }

    // Branch 6b: Inconclusive + PASS -> Inconclusive (preserving valid partial PASS).
    match run_matrix(&[pass_a, inc], &cfg) {
        DualMatrixDecision::Inconclusive { reason } => assert!(reason.contains("claude")),
        other => panic!("expected Branch 6b Inconclusive, got {other:?}"),
    }
}

#[test]
fn merge_deferred_outcome_enforces_strict_precedence() {
    let p_a = ProviderKey::from_executor("antigravity");
    let p_c = ProviderKey::from_executor("claude");
    let oauth = DeferredSiblingOutcome::RetryAfter {
        until: 999,
        attempt: 1,
        detail: "claude OAuth refreshed".into(),
        provider: p_c.clone(),
        oauth_refreshed: true,
    };
    let sw = DeferredSiblingOutcome::Switch {
        provider: p_a.clone(),
    };
    let wait = DeferredSiblingOutcome::Wait {
        until: Some(200),
        provider: p_a.clone(),
    };
    let retry = DeferredSiblingOutcome::RetryAfter {
        until: 150,
        attempt: 1,
        detail: "net".into(),
        provider: p_c.clone(),
        oauth_refreshed: false,
    };
    let blocked = DeferredSiblingOutcome::Blocked;
    let halt = DeferredSiblingOutcome::Halt("auth refused".into());

    // Strict precedence: OAuthRefreshed < Switch < Wait < RetryAfter(net) < Blocked < Halt.
    let chain = [&oauth, &sw, &wait, &retry, &blocked, &halt];
    for i in 0..chain.len() {
        for j in (i + 1)..chain.len() {
            assert_eq!(
                merge_deferred_outcome(Some(chain[i].clone()), chain[j].clone()),
                *chain[j]
            );
            assert_eq!(
                merge_deferred_outcome(Some(chain[j].clone()), chain[i].clone()),
                *chain[j]
            );
        }
    }
    assert_eq!(merge_deferred_outcome(None, sw.clone()), sw);

    let retry_later = DeferredSiblingOutcome::RetryAfter {
        until: 300,
        attempt: 2,
        detail: "net2".into(),
        provider: p_a.clone(),
        oauth_refreshed: false,
    };
    assert_eq!(
        merge_deferred_outcome(Some(retry), retry_later.clone()),
        retry_later
    );

    // When BOTH siblings are OAuthRefreshed, merged outcome remains oauth_refreshed: true.
    let oauth_agy = DeferredSiblingOutcome::RetryAfter {
        until: 1005,
        attempt: 1,
        detail: "agy OAuth refreshed".into(),
        provider: p_a.clone(),
        oauth_refreshed: true,
    };
    let both_oauth = merge_deferred_outcome(Some(oauth), oauth_agy);
    assert!(matches!(
        both_oauth,
        DeferredSiblingOutcome::RetryAfter {
            oauth_refreshed: true,
            until: 1005,
            ..
        }
    ));

    // When one sibling is OAuthRefreshed (even with a later timestamp) and the
    // other is a real network error, blame the non-OAuth failing provider.
    let retry_net_early = DeferredSiblingOutcome::RetryAfter {
        until: 1000,
        attempt: 1,
        detail: "antigravity connection reset".into(),
        provider: p_a.clone(),
        oauth_refreshed: false,
    };
    let retry_oauth_late = DeferredSiblingOutcome::RetryAfter {
        until: 1005,
        attempt: 1,
        detail: "claude OAuth refreshed".into(),
        provider: p_c.clone(),
        oauth_refreshed: true,
    };
    assert_eq!(
        merge_deferred_outcome(Some(retry_net_early.clone()), retry_oauth_late.clone()),
        retry_net_early
    );
    assert_eq!(
        merge_deferred_outcome(Some(retry_oauth_late), retry_net_early.clone()),
        retry_net_early
    );

    let wait_none = DeferredSiblingOutcome::Wait {
        until: None,
        provider: p_c,
    };
    assert_eq!(merge_deferred_outcome(Some(wait_none), wait.clone()), wait);
}

#[test]
fn decide_dual_matrix_p1_with_failing_score_gate_routes_directly_to_rework() {
    let cfg = dual_cfg(true, true, true);
    let fail_both = partial(
        "agy",
        "antigravity",
        "FAIL",
        Some([85, 98, 98, 98, 98, 98, 98]),
        vec![issue("A1", IssueSeverity::P1, "STUB", "src/auth.rs:10")],
    );
    let pass_b = partial("claude", "claude", "PASS", Some([98; 7]), vec![]);
    match run_matrix(&[fail_both, pass_b], &cfg) {
        DualMatrixDecision::Rework { findings, .. } => {
            assert!(findings.iter().any(|f| f.category == "STUB"));
            assert!(findings.iter().any(|f| f.category == "G1"));
            assert_eq!(findings.len(), 2);
        }
        other => panic!("expected direct Rework (never NeedsAdjudication), got {other:?}"),
    }
}

#[test]
fn telemetry_adjudicate_summary_and_handover_reviewer_id_uniqueness() {
    let line = summarize_event_one_line(
        EventKind::DualReviewTurn,
        r#"{"phase":"adjudicate","surviving":1,"dropped_p1":2}"#,
    );
    assert_eq!(
        line,
        "dual_review_turn: phase=adjudicate surviving=1 dropped_p1=2"
    );

    let r1 = verdict_rec("dup", "antigravity", Some([96; 7]));
    let r2 = verdict_rec("dup", "claude", Some([98; 7]));
    let dup_rec = build_dual_audit_record("/repo", "fp", "cfg", 100, vec![r1, r2], false, 1);
    let dup_missing = check_dual_review_missing(Some(&dup_rec), true, true, true, 95, 90);
    assert!(
        dup_missing.iter().any(|m| m.contains("duplicate")),
        "expected duplicate id failure, got {dup_missing:?}"
    );

    let e1 = verdict_rec("  ", "antigravity", Some([96; 7]));
    let e2 = verdict_rec("claude", "claude", Some([98; 7]));
    let empty_rec = build_dual_audit_record("/repo", "fp", "cfg", 100, vec![e1, e2], false, 1);
    let empty_missing = check_dual_review_missing(Some(&empty_rec), true, true, true, 95, 90);
    assert!(
        empty_missing.iter().any(|m| m.contains("empty id")),
        "expected empty id failure, got {empty_missing:?}"
    );
}
