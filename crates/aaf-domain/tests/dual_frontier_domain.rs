#![allow(clippy::unwrap_used, clippy::expect_used)]

use aaf_domain::config::{
    CrossAuditConfig, GlobalConfig, GlobalCrossAuditConfig, GlobalPipelineConfig,
    GlobalUnblockerConfig, MAX_PLAN_ROUNDS_CAP, PipelineConfig, ReviewerConfig, StageModelConfig,
};
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::gate::{GoldenScore, PostGateAuditRecord};
use aaf_domain::pipeline::{
    AdjudicateDecision, AdjudicateResponse, AdjudicateVerdict,
    DEFAULT_ESCALATE_IMPLEMENTER_AFTER_REWORKS, DEFAULT_MAX_PLAN_ROUNDS, DispositionAction,
    IssueSeverity, MAX_PLAN_CONTRACT_BYTES, PlanChecklistItem, PlanContract, PlanCritique,
    PlanCritiqueVerdict, PlanDisposition, PlanFileChange, RedGreenTestPlan, ResolvedIssue,
    ResolvedStatus, ReviewIssue, ReviewerVerdictRecord, TaskTier, canonical_provider,
    distinct_real_canonical_providers, is_known_real_provider, validate_reviewer_id,
    validate_reviewers,
};
use aaf_domain::ports::{AgentRequest, EventSink, PortResult, SandboxPolicy};
use serde_json::json;

#[test]
fn pipeline_constants_match_contract() {
    assert_eq!(MAX_PLAN_ROUNDS_CAP, 5);
    assert_eq!(DEFAULT_MAX_PLAN_ROUNDS, 3);
    assert_eq!(DEFAULT_ESCALATE_IMPLEMENTER_AFTER_REWORKS, 1);
    assert_eq!(MAX_PLAN_CONTRACT_BYTES, 32 * 1024);
}

#[test]
fn canonical_provider_and_distinct_counts_exclude_custom_and_deduplicate_gemini() {
    assert_eq!(canonical_provider("antigravity"), "gemini");
    assert_eq!(canonical_provider("  Antigravity "), "gemini");
    assert_eq!(canonical_provider("GEMINI"), "gemini");
    assert_eq!(canonical_provider("Claude"), "claude");
    assert_eq!(canonical_provider("custom"), "custom");

    for known in [
        "claude",
        "codex",
        "gemini",
        "antigravity",
        "opencode",
        "cursor",
        "kimi",
        "pi",
        "codebuddy",
        "deepseek",
        " CLAUDE ",
        "Antigravity",
    ] {
        assert!(is_known_real_provider(known), "expected real: {known:?}");
    }
    for unknown in ["", "   ", "custom", "CUSTOM", "fake-wrapper", "local-sh"] {
        assert!(
            !is_known_real_provider(unknown),
            "expected excluded: {unknown:?}"
        );
    }

    // antigravity + gemini collapse to one canonical provider ("gemini")
    assert_eq!(
        distinct_real_canonical_providers(["antigravity", "gemini"]),
        1
    );
    // custom, empty, unknown do not inflate distinct count
    assert_eq!(
        distinct_real_canonical_providers(["claude", "custom", "", "unknown"]),
        1
    );
    // claude + antigravity = 2 independent real canonical providers
    assert_eq!(
        distinct_real_canonical_providers(["claude", "antigravity"]),
        2
    );
    assert_eq!(
        distinct_real_canonical_providers(["claude", "antigravity", "gemini", "codex", "custom"]),
        3
    );
}

#[test]
fn validate_reviewer_id_accepts_safe_ids_and_rejects_traversal_and_bad_chars() {
    for valid in [
        "a",
        "claude-primary",
        "reviewer_01",
        "A-Z_0-9",
        &"x".repeat(64),
    ] {
        assert!(
            validate_reviewer_id(valid).is_ok(),
            "expected valid id: {valid:?}"
        );
    }

    let too_long = "a".repeat(65);
    for invalid in [
        "",
        too_long.as_str(),
        "..",
        "../receipts",
        "a/b",
        "a\\b",
        "has space",
        "has.dot",
        "null\0byte",
        "line\nbreak",
        "non_ascii_é",
    ] {
        assert!(
            validate_reviewer_id(invalid).is_err(),
            "expected invalid id: {invalid:?}"
        );
    }
}

#[test]
fn validate_reviewers_rejects_duplicates_and_invalid_ids() {
    let rev = |id: &str| ReviewerConfig {
        id: id.to_string(),
        executor: "claude".to_string(),
        ..ReviewerConfig::default()
    };

    assert!(validate_reviewers(&[]).is_ok());
    assert!(validate_reviewers(&[rev("rev-a"), rev("rev-b")]).is_ok());
    assert!(validate_reviewers(&[rev("rev-a"), rev("rev-a")]).is_err());
    assert!(validate_reviewers(&[rev("rev-a"), rev("../escape")]).is_err());
    assert!(validate_reviewers(&[rev("")]).is_err());
}

#[test]
fn config_defaults_and_stage_model_is_empty() {
    let stage = StageModelConfig::default();
    assert!(stage.is_empty());
    let whitespace_only = StageModelConfig {
        executor: "  ".to_string(),
        executor_cmd: String::new(),
        model: "\t".to_string(),
        effort: String::new(),
    };
    assert!(whitespace_only.is_empty());
    let populated = StageModelConfig {
        executor: "claude".to_string(),
        ..StageModelConfig::default()
    };
    assert!(!populated.is_empty());

    let pipeline = PipelineConfig::default();
    assert!(!pipeline.plan_stage);
    assert!(!pipeline.plan_debate);
    assert!(!pipeline.require_plan_approval);
    assert_eq!(pipeline.max_plan_rounds, 3);
    assert!(!pipeline.allow_tier_s_skip_debate);
    assert_eq!(pipeline.max_audit_reworks, 3);
    assert_eq!(pipeline.escalate_implementer_after_reworks, 1);
    assert!(pipeline.plan_author.is_empty());
    assert!(pipeline.plan_challenger.is_empty());
    assert!(pipeline.implementer.is_empty());
    assert!(pipeline.implementer_escalated.is_empty());

    let cross = CrossAuditConfig::default();
    assert!(!cross.dual_review);
    assert!(cross.cross_adjudicate);
    assert!(cross.reviewers.is_empty());

    let global = GlobalConfig::default();
    assert_eq!(global.pipeline, GlobalPipelineConfig::default());
    assert_eq!(global.unblocker, GlobalUnblockerConfig::default());
    assert_eq!(global.cross_audit, GlobalCrossAuditConfig::default());
}

#[test]
fn legacy_post_gate_audit_record_deserializes_with_defaults() {
    let legacy_json = json!({
        "repo_root": "/repo",
        "tree_fp": "git-tree:abc",
        "config_hash": "cfg123",
        "verdict": "PASS",
        "reviewer": "codex",
        "same_account": false,
        "mechanical_score": 100,
        "llm_average": 97,
        "effective_score": 97,
        "scores": [
            {"id": "G1", "score": 98, "deduction_reason": ""}
        ],
        "created_at": 1_700_000_000
    });

    let rec: PostGateAuditRecord = serde_json::from_value(legacy_json).unwrap();
    assert!(!rec.dual_consensus);
    assert!(!rec.adjudicated);
    assert!(rec.reviewers.is_empty());
    assert_eq!(rec.verdict, "PASS");
    assert_eq!(rec.effective_score, 97);
}

#[test]
fn post_gate_audit_record_round_trips_with_dual_reviewers() {
    let rec = PostGateAuditRecord {
        repo_root: "/repo".to_string(),
        tree_fp: "git-tree:abc".to_string(),
        config_hash: "cfg123".to_string(),
        verdict: "PASS".to_string(),
        reviewer: "dual".to_string(),
        same_account: false,
        mechanical_score: 100,
        llm_average: Some(96),
        effective_score: 96,
        scores: vec![GoldenScore {
            id: "G1".to_string(),
            score: 96,
            deduction_reason: String::new(),
        }],
        dual_consensus: true,
        adjudicated: true,
        reviewers: vec![ReviewerVerdictRecord {
            id: "rev-a".to_string(),
            executor: "antigravity".to_string(),
            canonical_provider: "gemini".to_string(),
            model: "model-a".to_string(),
            effort: "high".to_string(),
            verdict: "PASS".to_string(),
            p0: 0,
            p1: 0,
            p2: 1,
            llm_average: Some(96),
            scores: vec![GoldenScore {
                id: "G1".to_string(),
                score: 96,
                deduction_reason: String::new(),
            }],
        }],
        created_at: 1_700_000_001,
    };

    let wire = serde_json::to_string(&rec).unwrap();
    let back: PostGateAuditRecord = serde_json::from_str(&wire).unwrap();
    assert_eq!(back, rec);
}

#[test]
fn plan_debate_and_adjudication_wire_shapes() {
    assert_eq!(TaskTier::S.as_str(), "S");
    assert_eq!(TaskTier::M.as_str(), "M");
    assert_eq!(TaskTier::L.as_str(), "L");
    assert_eq!(serde_json::to_value(TaskTier::L).unwrap(), json!("L"));

    let contract = PlanContract {
        summary: "Add feature".to_string(),
        tier_assessment: "M".to_string(),
        tier_reasons: vec!["2 files".to_string()],
        files_to_change: vec![PlanFileChange {
            path: "src/lib.rs".to_string(),
            responsibility: "core logic".to_string(),
            risk: "low".to_string(),
        }],
        invariants_and_edge_cases: vec!["empty input".to_string()],
        red_green_tests: vec![RedGreenTestPlan {
            ac_ref: "AC-1".to_string(),
            test_path: "tests/unit.rs".to_string(),
            red_condition: "returns Err".to_string(),
            green_condition: "returns Ok".to_string(),
        }],
        dispositions: vec![PlanDisposition {
            issue_id: "ISS-1".to_string(),
            action: DispositionAction::Accepted,
            response: "Updated plan".to_string(),
            evidence: "src/lib.rs:10".to_string(),
        }],
        open_questions: vec![],
    };
    let contract_wire = serde_json::to_value(&contract).unwrap();
    assert_eq!(
        contract_wire["dispositions"][0]["action"],
        json!("ACCEPTED")
    );
    let contract_back: PlanContract = serde_json::from_value(contract_wire).unwrap();
    assert_eq!(contract_back, contract);

    let critique = PlanCritique {
        checklist: vec![PlanChecklistItem {
            id: "PLAN-SEC".to_string(),
            pass: true,
            evidence: "checked".to_string(),
        }],
        resolved_from_previous: vec![ResolvedIssue {
            issue_id: "ISS-1".to_string(),
            status: ResolvedStatus::VerifiedFixed,
            reason: "verified".to_string(),
        }],
        issues: vec![ReviewIssue {
            id: "ISS-2".to_string(),
            severity: IssueSeverity::P2,
            category: "STYLE".to_string(),
            location: "src/lib.rs:12".to_string(),
            description: "minor nit".to_string(),
            proposed_fix: "rename var".to_string(),
        }],
        verdict: PlanCritiqueVerdict::Converged,
    };
    let critique_wire = serde_json::to_value(&critique).unwrap();
    assert_eq!(critique_wire["verdict"], json!("CONVERGED"));
    assert_eq!(
        critique_wire["resolved_from_previous"][0]["status"],
        json!("VERIFIED_FIXED")
    );
    assert_eq!(critique_wire["issues"][0]["severity"], json!("P2"));
    let critique_back: PlanCritique = serde_json::from_value(critique_wire).unwrap();
    assert_eq!(critique_back, critique);

    let adj = AdjudicateResponse {
        verdicts: vec![
            AdjudicateVerdict {
                issue_id: "ISS-1".to_string(),
                decision: AdjudicateDecision::Concur,
                evidence: "src/lib.rs:42".to_string(),
                reason: "valid bug".to_string(),
            },
            AdjudicateVerdict {
                issue_id: "ISS-2".to_string(),
                decision: AdjudicateDecision::Rebut,
                evidence: "src/lib.rs:50".to_string(),
                reason: "already guarded".to_string(),
            },
        ],
    };
    let adj_wire = serde_json::to_value(&adj).unwrap();
    assert_eq!(adj_wire["verdicts"][0]["decision"], json!("CONCUR"));
    assert_eq!(adj_wire["verdicts"][1]["decision"], json!("REBUT"));
    let adj_back: AdjudicateResponse = serde_json::from_value(adj_wire).unwrap();
    assert_eq!(adj_back, adj);
}

#[test]
fn agent_request_legacy_json_defaults_json_schema_to_none() {
    let legacy = json!({
        "run_id": "r1",
        "prompt": "hello",
        "cwd": "/tmp/wt",
        "account": null,
        "timeout_secs": 60,
        "sandbox": SandboxPolicy::default()
    });
    let req: AgentRequest = serde_json::from_value(legacy).unwrap();
    assert_eq!(req.json_schema, None);
}

struct FakeSink(Vec<FactoryEvent>);

impl EventSink for FakeSink {
    fn emit(&self, _event: &FactoryEvent) -> PortResult<i64> {
        Ok(1)
    }

    fn events_for_task(&self, task_id: &str, limit: u32) -> PortResult<Vec<FactoryEvent>> {
        Ok(self
            .0
            .iter()
            .filter(|ev| ev.task_id.as_deref() == Some(task_id))
            .take(limit as usize)
            .cloned()
            .collect())
    }
}

#[test]
fn event_sink_default_events_for_task_by_kinds_filters_and_limits() {
    let sink = FakeSink(vec![
        FactoryEvent::new(EventKind::PlanConverged, "{}", 30).for_task("t1"),
        FactoryEvent::new(EventKind::GateResult, "{}", 20).for_task("t1"),
        FactoryEvent::new(EventKind::PlanDebateTurn, "{}", 15).for_task("t1"),
        FactoryEvent::new(EventKind::PlanDebateTurn, "{}", 10).for_task("t1"),
        FactoryEvent::new(EventKind::PlanConverged, "{}", 5).for_task("t2"),
    ]);

    let filtered = sink
        .events_for_task_by_kinds(
            "t1",
            &[EventKind::PlanConverged, EventKind::PlanDebateTurn],
            2,
        )
        .unwrap();
    assert_eq!(filtered.len(), 2);
    assert_eq!(filtered[0].kind, EventKind::PlanConverged);
    assert_eq!(filtered[1].kind, EventKind::PlanDebateTurn);
    assert_eq!(filtered[1].created_at, 15);
}
