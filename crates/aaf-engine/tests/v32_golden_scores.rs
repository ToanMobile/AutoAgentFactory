#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Tests for the 7 Golden Standards rubric, strict scored verdict parsing,
//! deterministic mechanical scoring, score-gate evaluation, and `run_scored_audit`.

use std::sync::Mutex;

use aaf_domain::gate::{
    GateMode, GateStatus, GoldenScore, HeadLockReport, LockStatus, RECEIPT_SCHEMA_VERSION, Receipt,
    Severity, StaticFinding, StoreComplianceReport, StoreStatus,
};
use aaf_domain::ports::{AgentOutcome, AgentRequest, AgentRunner, PortResult, SandboxPolicy};
use aaf_domain::task::ImmutableSpec;
use aaf_engine::cross_audit::prompt::DIFF_END;
use aaf_engine::cross_audit::{
    AuditInput, AuditVerdict, GOLDEN_IDS, GOLDEN_STANDARDS, Item, MechanicalScoreReport,
    build_checklist, build_prompt, build_scored_prompt, compute_mechanical_score,
    evaluate_score_gate, parse_scored_verdict, run_audit, run_scored_audit,
};

struct FakeRunner {
    text: String,
    seen: Mutex<Vec<AgentRequest>>,
}

impl FakeRunner {
    fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            seen: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<AgentRequest> {
        self.seen.lock().unwrap().clone()
    }
}

impl AgentRunner for FakeRunner {
    fn run(&self, request: &AgentRequest) -> PortResult<AgentOutcome> {
        self.seen.lock().unwrap().push(request.clone());
        Ok(AgentOutcome {
            exit_code: Some(0),
            timed_out: false,
            final_text: self.text.clone(),
            tokens_in: 200,
            tokens_out: 80,
            limit_hit: None,
            infra: None,
            log_tail: String::new(),
        })
    }
}

fn spec() -> ImmutableSpec {
    ImmutableSpec {
        description: "Add deterministic golden-standard scoring".to_string(),
        acceptance: vec!["Computes mechanical score from signed receipt".to_string()],
    }
}

fn audit_input(diff: &str) -> AuditInput {
    AuditInput {
        spec: spec(),
        diff: diff.to_string(),
        mobile: false,
    }
}

fn request_template() -> AgentRequest {
    AgentRequest {
        run_id: "audit-v32".to_string(),
        prompt: "placeholder".to_string(),
        cwd: "/work/tree".to_string(),
        account: None,
        timeout_secs: 300,
        sandbox: SandboxPolicy {
            enabled: true,
            read_only: false,
            writable_paths: vec![],
            read_denied_paths: vec!["/data".to_string()],
        },
    }
}

fn answers_json(checklist: &[Item], yes: &[&str]) -> Vec<serde_json::Value> {
    checklist
        .iter()
        .map(|item| {
            let hit = yes.contains(&item.id.as_str());
            serde_json::json!({
                "id": item.id,
                "answer": if hit { "yes" } else { "no" },
                "evidence": if hit { "src/lib.rs:12" } else { "" },
            })
        })
        .collect()
}

fn full_scores_json(pairs: &[(&str, u8, &str)]) -> Vec<serde_json::Value> {
    pairs
        .iter()
        .map(|(id, score, reason)| {
            serde_json::json!({
                "id": id,
                "score": score,
                "deduction_reason": reason,
            })
        })
        .collect()
}

fn clean_receipt() -> Receipt {
    Receipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        repo_root: "/repo".to_string(),
        tree_fp: "abc123".to_string(),
        head_sha: Some("deadbeef".to_string()),
        base_sha: Some("cafebabe".to_string()),
        mode: GateMode::Scoped,
        status: GateStatus::Clear,
        reasons: vec![],
        config_hash: "cfg-1".to_string(),
        commands: vec![],
        static_findings: vec![],
        suppressions: 0,
        head_lock: HeadLockReport {
            status: LockStatus::Clear,
            lock_fp: String::new(),
            locked_changes: vec![],
            approval: None,
        },
        store_compliance: StoreComplianceReport::not_applicable(),
        created_at: 1_700_000_000,
    }
}

fn static_finding(severity: Severity) -> StaticFinding {
    StaticFinding {
        rule: "SEC-001".to_string(),
        severity,
        file: "src/lib.rs".to_string(),
        line: Some(42),
        message: "finding".to_string(),
        fix: "fix it".to_string(),
    }
}

fn golden_scores(values: [u8; 7]) -> Vec<GoldenScore> {
    GOLDEN_IDS
        .iter()
        .zip(values)
        .map(|(id, score)| GoldenScore {
            id: (*id).to_string(),
            score,
            deduction_reason: if score < 100 {
                format!("minor deduction on {id}")
            } else {
                String::new()
            },
        })
        .collect()
}

#[test]
fn golden_constants_cover_g1_through_g7_in_order() {
    assert_eq!(GOLDEN_IDS, ["G1", "G2", "G3", "G4", "G5", "G6", "G7"]);
    let std_ids: Vec<&str> = GOLDEN_STANDARDS.iter().map(|(id, _)| *id).collect();
    assert_eq!(std_ids, GOLDEN_IDS);
    assert!(
        GOLDEN_STANDARDS
            .iter()
            .all(|(_, desc)| !desc.trim().is_empty())
    );
}

#[test]
fn parse_scored_verdict_accepts_valid_g1_to_g7_and_orders_canonically() {
    let checklist = build_checklist(&spec(), false);
    // Provide scores in reverse order, mixing 100 and <100 with reason.
    let scores = full_scores_json(&[
        ("G7", 100, ""),
        ("G6", 96, "small helper could be clearer"),
        ("G5", 100, ""),
        ("G4", 98, "one extra boundary case"),
        ("G3", 100, ""),
        ("G2", 100, ""),
        ("G1", 95, "module split slightly wide"),
    ]);
    let payload = serde_json::json!({
        "answers": answers_json(&checklist, &[]),
        "scores": scores,
    })
    .to_string();

    let parsed = parse_scored_verdict(&payload, &checklist, true);
    assert_eq!(parsed.verdict, AuditVerdict::Pass { warnings: vec![] });
    let ids: Vec<&str> = parsed.scores.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, GOLDEN_IDS);
    assert_eq!(parsed.scores[0].score, 95);
    assert_eq!(
        parsed.scores[0].deduction_reason,
        "module split slightly wide"
    );
    assert_eq!(parsed.scores[6].score, 100);
}

#[test]
fn parse_scored_verdict_rejects_missing_duplicate_unknown_and_invalid_scores() {
    let checklist = build_checklist(&spec(), false);
    let base_answers = answers_json(&checklist, &[]);
    let valid_scores = full_scores_json(&[
        ("G1", 100, ""),
        ("G2", 100, ""),
        ("G3", 100, ""),
        ("G4", 100, ""),
        ("G5", 100, ""),
        ("G6", 100, ""),
        ("G7", 100, ""),
    ]);

    let cases = [
        // Missing scores field when require_scores = true.
        serde_json::json!({ "answers": base_answers }),
        // Missing G7 (only 6 entries).
        serde_json::json!({ "answers": base_answers, "scores": &valid_scores[..6] }),
        // Duplicate G1 (even with 7 total entries).
        serde_json::json!({
            "answers": base_answers,
            "scores": full_scores_json(&[
                ("G1", 100, ""), ("G1", 100, ""), ("G3", 100, ""),
                ("G4", 100, ""), ("G5", 100, ""), ("G6", 100, ""), ("G7", 100, ""),
            ]),
        }),
        // Unknown G8.
        serde_json::json!({
            "answers": base_answers,
            "scores": full_scores_json(&[
                ("G1", 100, ""), ("G2", 100, ""), ("G3", 100, ""),
                ("G4", 100, ""), ("G5", 100, ""), ("G6", 100, ""), ("G8", 100, ""),
            ]),
        }),
        // Score > 100.
        serde_json::json!({
            "answers": base_answers,
            "scores": full_scores_json(&[
                ("G1", 101, ""), ("G2", 100, ""), ("G3", 100, ""),
                ("G4", 100, ""), ("G5", 100, ""), ("G6", 100, ""), ("G7", 100, ""),
            ]),
        }),
        // Score < 100 with empty deduction_reason.
        serde_json::json!({
            "answers": base_answers,
            "scores": full_scores_json(&[
                ("G1", 95, ""), ("G2", 100, ""), ("G3", 100, ""),
                ("G4", 100, ""), ("G5", 100, ""), ("G6", 100, ""), ("G7", 100, ""),
            ]),
        }),
        // Score < 100 with whitespace-only deduction_reason.
        serde_json::json!({
            "answers": base_answers,
            "scores": full_scores_json(&[
                ("G1", 95, "   \t\n "), ("G2", 100, ""), ("G3", 100, ""),
                ("G4", 100, ""), ("G5", 100, ""), ("G6", 100, ""), ("G7", 100, ""),
            ]),
        }),
        // Unknown extra top-level JSON field rejected by deny_unknown_fields.
        serde_json::json!({ "answers": base_answers, "scores": valid_scores, "extra": true }),
        // Unknown extra JSON field inside a score item rejected by deny_unknown_fields.
        serde_json::json!({
            "answers": base_answers,
            "scores": [
                {"id": "G1", "score": 100, "deduction_reason": "", "note": "unexpected"},
                {"id": "G2", "score": 100, "deduction_reason": ""},
                {"id": "G3", "score": 100, "deduction_reason": ""},
                {"id": "G4", "score": 100, "deduction_reason": ""},
                {"id": "G5", "score": 100, "deduction_reason": ""},
                {"id": "G6", "score": 100, "deduction_reason": ""},
                {"id": "G7", "score": 100, "deduction_reason": ""},
            ],
        }),
    ];

    for case in cases {
        let parsed = parse_scored_verdict(&case.to_string(), &checklist, true);
        assert!(
            matches!(parsed.verdict, AuditVerdict::Inconclusive { .. }),
            "expected Inconclusive for {case}"
        );
        assert!(parsed.scores.is_empty());
    }
}

#[test]
fn parse_scored_verdict_backward_compatible_when_scores_not_required() {
    let checklist = build_checklist(&spec(), false);
    let answers_only = serde_json::json!({
        "answers": answers_json(&checklist, &[]),
    })
    .to_string();

    let parsed = parse_scored_verdict(&answers_only, &checklist, false);
    assert_eq!(parsed.verdict, AuditVerdict::Pass { warnings: vec![] });
    assert!(parsed.scores.is_empty());

    // Malformed `scores` when present still fails closed even if require_scores = false.
    let bad_optional = serde_json::json!({
        "answers": answers_json(&checklist, &[]),
        "scores": [{"id": "G1", "score": 90, "deduction_reason": ""}],
    })
    .to_string();
    assert!(matches!(
        parse_scored_verdict(&bad_optional, &checklist, false).verdict,
        AuditVerdict::Inconclusive { .. }
    ));
}

#[test]
fn mechanical_score_clean_and_hard_zero_conditions() {
    let clean = clean_receipt();
    let report = compute_mechanical_score(&clean, true, false, false);
    assert_eq!(report.score, 100);
    assert!(report.reasons.is_empty());
    assert!(!report.same_account_warning);

    // Non-Clear status -> 0.
    for status in [GateStatus::Blocked, GateStatus::Unverified] {
        let mut r = clean_receipt();
        r.status = status;
        let rep = compute_mechanical_score(&r, true, false, false);
        assert_eq!(rep.score, 0);
        assert!(!rep.reasons.is_empty());
    }

    // Critical static finding -> 0.
    let mut crit_static = clean_receipt();
    crit_static
        .static_findings
        .push(static_finding(Severity::Critical));
    let rep = compute_mechanical_score(&crit_static, true, false, false);
    assert_eq!(rep.score, 0);
    assert!(rep.reasons.iter().any(|r| r.contains("static")));

    // Critical store compliance -> 0.
    let mut crit_store = clean_receipt();
    crit_store.store_compliance.status = StoreStatus::Blocked;
    crit_store.store_compliance.critical = 1;
    let rep = compute_mechanical_score(&crit_store, true, false, false);
    assert_eq!(rep.score, 0);
    assert!(rep.reasons.iter().any(|r| r.contains("store")));

    // !proof_ok -> 0 (and preserves same_account_warning when applicable).
    let rep = compute_mechanical_score(&clean, false, false, true);
    assert_eq!(rep.score, 0);
    assert!(rep.same_account_warning);
    assert!(rep.reasons.iter().any(|r| r.contains("proof")));
}

#[test]
fn mechanical_score_deductions_caps_and_same_account_rules() {
    // Warnings: -2 each, cap -10 (static Warn + store_compliance.warnings).
    let mut r = clean_receipt();
    r.static_findings = vec![
        static_finding(Severity::Warn),
        static_finding(Severity::Warn),
        static_finding(Severity::Info),
    ];
    r.store_compliance.warnings = 1;
    let rep = compute_mechanical_score(&r, true, false, false);
    assert_eq!(rep.score, 94);
    assert_eq!(rep.reasons.len(), 1);

    r.store_compliance.warnings = 10;
    let rep = compute_mechanical_score(&r, true, false, false);
    assert_eq!(rep.score, 90);

    // Suppressions: -1 each, cap -5.
    let mut r = clean_receipt();
    r.suppressions = 3;
    assert_eq!(compute_mechanical_score(&r, true, false, false).score, 97);
    r.suppressions = 9;
    assert_eq!(compute_mechanical_score(&r, true, false, false).score, 95);

    // Unresolved store items: -3 each, cap -12.
    let mut r = clean_receipt();
    r.store_compliance.unresolved = vec!["a".into(), "b".into()];
    assert_eq!(compute_mechanical_score(&r, true, false, false).score, 94);
    r.store_compliance.unresolved = vec!["a".into(); 6];
    assert_eq!(compute_mechanical_score(&r, true, false, false).score, 88);

    // Combined capped deductions: 100 - 10 - 5 - 12 = 73.
    let mut combined = clean_receipt();
    combined.static_findings = vec![static_finding(Severity::Warn); 6];
    combined.suppressions = 7;
    combined.store_compliance.unresolved = vec!["x".into(); 5];
    let rep = compute_mechanical_score(&combined, true, false, false);
    assert_eq!(rep.score, 73);
    assert_eq!(rep.reasons.len(), 3);

    // Solo-dev non-deadlocking rule:
    // require_different_provider=true && same_account=true -> cap 94, warning=false.
    let clean = clean_receipt();
    let strict_same = compute_mechanical_score(&clean, true, true, true);
    assert_eq!(strict_same.score, 94);
    assert!(!strict_same.same_account_warning);
    assert!(
        strict_same
            .reasons
            .iter()
            .any(|m| m.contains("require_different_provider=true with same_account"))
    );

    // require_different_provider=false && same_account=true -> NOT capped (100), warning=true.
    let solo_same = compute_mechanical_score(&clean, true, false, true);
    assert_eq!(solo_same.score, 100);
    assert!(solo_same.same_account_warning);
    assert!(solo_same.reasons.is_empty());
}

#[test]
fn evaluate_score_gate_checks_effective_average_criterion_floor_and_coverage() {
    let mech_100 = MechanicalScoreReport {
        score: 100,
        reasons: vec![],
        same_account_warning: false,
    };

    // Pass: min(100, 96) >= 95 and all G1..G7 >= 90.
    let scores = golden_scores([95, 96, 97, 96, 98, 95, 96]);
    let eval = evaluate_score_gate(&mech_100, &scores, 95, 90);
    assert!(eval.passed);
    assert_eq!(eval.mechanical_score, 100);
    assert_eq!(eval.llm_average, Some(96));
    assert_eq!(eval.effective_score, 96);
    assert!(eval.reasons.is_empty());

    // Fail: one criterion is 89 even though average is 98 (689 / 7 = 98).
    let dipped = golden_scores([89, 100, 100, 100, 100, 100, 100]);
    let eval = evaluate_score_gate(&mech_100, &dipped, 95, 90);
    assert!(!eval.passed);
    assert_eq!(eval.llm_average, Some(98));
    assert_eq!(eval.effective_score, 98);
    assert!(
        eval.reasons
            .iter()
            .any(|r| r.contains("G1") && r.contains("89"))
    );

    // Fail: mechanical 94 with LLM avg 99 -> effective 94 < 95.
    let mech_94 = MechanicalScoreReport {
        score: 94,
        reasons: vec!["capped".to_string()],
        same_account_warning: false,
    };
    let high_llm = golden_scores([99, 99, 99, 99, 99, 99, 100]);
    let eval = evaluate_score_gate(&mech_94, &high_llm, 95, 90);
    assert!(!eval.passed);
    assert_eq!(eval.llm_average, Some(99));
    assert_eq!(eval.effective_score, 94);
    assert!(eval.reasons.iter().any(|r| r.contains("94")));

    // Fail closed on empty or incomplete scores.
    let empty_eval = evaluate_score_gate(&mech_100, &[], 95, 90);
    assert!(!empty_eval.passed);
    assert_eq!(empty_eval.llm_average, None);
    assert_eq!(empty_eval.effective_score, 0);
    assert!(!empty_eval.reasons.is_empty());

    let short_eval = evaluate_score_gate(&mech_100, &scores[..6], 95, 90);
    assert!(!short_eval.passed);
    assert_eq!(short_eval.llm_average, None);
    assert_eq!(short_eval.effective_score, 0);
}

#[test]
fn run_scored_audit_end_to_end_and_prompt_contract() {
    let input = audit_input("+pub fn score() -> u8 { 100 }");
    assert_eq!(build_scored_prompt(&input, false), build_prompt(&input));

    let scored_prompt = build_scored_prompt(&input, true);
    let diff_end = scored_prompt.find(DIFF_END).unwrap();
    for id in GOLDEN_IDS {
        let pos = scored_prompt.find(&format!("- {id}: ")).unwrap();
        assert!(pos > diff_end, "{id} rubric must appear after DIFF_END");
    }
    assert!(scored_prompt.contains("\"scores\""));
    assert!(scored_prompt.contains("\"deduction_reason\""));

    let checklist = build_checklist(&input.spec, input.mobile);
    let scored_reply = serde_json::json!({
        "answers": answers_json(&checklist, &[]),
        "scores": full_scores_json(&[
            ("G1", 100, ""),
            ("G2", 100, ""),
            ("G3", 98, "minor edge note"),
            ("G4", 100, ""),
            ("G5", 100, ""),
            ("G6", 100, ""),
            ("G7", 100, ""),
        ]),
    })
    .to_string();

    let runner = FakeRunner::new(scored_reply);
    let report = run_scored_audit(&input, &runner, &request_template(), true).unwrap();
    assert_eq!(report.verdict, AuditVerdict::Pass { warnings: vec![] });
    assert_eq!(report.scores.len(), 7);
    assert_eq!(report.scores[2].id, "G3");
    assert_eq!(report.scores[2].score, 98);
    assert_eq!(runner.requests()[0].prompt, scored_prompt);

    // Missing scores under require_scores=true -> Inconclusive.
    let answers_only = serde_json::json!({
        "answers": answers_json(&checklist, &[]),
    })
    .to_string();
    let runner_missing = FakeRunner::new(answers_only.clone());
    let missing_report =
        run_scored_audit(&input, &runner_missing, &request_template(), true).unwrap();
    assert!(matches!(
        missing_report.verdict,
        AuditVerdict::Inconclusive { .. }
    ));
    assert!(missing_report.scores.is_empty());

    // Legacy run_audit keeps answers-only JSON passing with empty scores.
    let runner_legacy = FakeRunner::new(answers_only);
    let legacy_report = run_audit(&input, &runner_legacy, &request_template()).unwrap();
    assert_eq!(
        legacy_report.verdict,
        AuditVerdict::Pass { warnings: vec![] }
    );
    assert!(legacy_report.scores.is_empty());
}
