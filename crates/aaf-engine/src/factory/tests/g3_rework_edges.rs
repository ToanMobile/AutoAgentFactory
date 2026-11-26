//! Edge-case and adversarial-review regression tests for G3 rework finding
//! filtering and score collection.

use aaf_domain::config::CrossAuditConfig;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::gate::GoldenScore;
use aaf_domain::ports::{BudgetStore, EventSink, TaskStore};
use aaf_domain::task::{Task, TaskState};
use serde_json::json;

use super::fakes::spec;
use super::world::World;
use crate::cross_audit::{AuditVerdict, Finding, build_checklist};
use crate::factory::post_audit::collect_rework_findings;
use crate::factory::rework_filter::{fallback_verdict_summary, has_file_line_ref};
use crate::factory::unblock::handle_audit_failure;
use crate::handover::build_handover_from_parts;

fn add_gating_task(world: &World, id: &str) -> Task {
    world.add(id);
    world.db.set_state(id, TaskState::Running, None, 1).unwrap();
    world.db.set_state(id, TaskState::Gating, None, 2).unwrap();
    world.db.get_task(id).unwrap()
}

fn finding(id: &str, question: &str, evidence: &str) -> Finding {
    Finding {
        id: id.to_string(),
        question: question.to_string(),
        evidence: evidence.to_string(),
    }
}

#[test]
fn prose_only_checklist_finding_is_dropped_and_audited() {
    let world = World::new();
    let task = add_gating_task(&world, "t1");
    let deps = world.deps();

    let findings = vec![
        finding("STUB", "left a placeholder?", "looks unfinished overall"),
        finding(
            "GATE-CI",
            "weakened gate?",
            "removed checks at https://example.com:443 at 12:30",
        ),
        finding("SECRET", "added secret?", "note:1 or src/lib.rs:0"),
    ];
    let next = handle_audit_failure(&deps, &task, &findings).unwrap();

    assert!(next.is_none());
    assert_eq!(world.state("t1"), TaskState::Gating);
    assert!(world.task("t1").context_notes.is_empty());
    assert!(world.db.get_budget("t1").is_err());

    let events = world.db.events_for_task("t1", 50).unwrap();
    let dropped_ev = events
        .iter()
        .find(|e| {
            e.kind == EventKind::DetectorHit && e.payload_json.contains("audit_rework_filter")
        })
        .expect("dropped findings must be recorded in DetectorHit event");
    let payload: serde_json::Value = serde_json::from_str(&dropped_ev.payload_json).unwrap();
    assert_eq!(payload["dropped_count"], 3);
    let ids: Vec<&str> = payload["dropped"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["id"].as_str())
        .collect();
    assert_eq!(ids, vec!["STUB", "GATE-CI", "SECRET"]);
    assert!(
        payload["dropped"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["reason"] == "missing_file_line")
    );
}

#[test]
fn mixed_findings_keep_file_line_variants_and_record_dropped_reason() {
    let world = World::new();
    let task = add_gating_task(&world, "t1");
    let deps = world.deps();

    let findings = vec![
        finding("ASSERT", "weakened?", "`crates/a/src/lib.rs:42-48` removed"),
        finding("STUB", "stubbed?", "see (foo.rs:12:3)."),
        finding("HARDCODED", "hardcoded?", "   "),
        finding("AC-1", "missed ac?", "no location cited"),
    ];
    let next = handle_audit_failure(&deps, &task, &findings).unwrap();

    assert!(next.is_some());
    assert_eq!(world.state("t1"), TaskState::Ready);
    // Audit rework rounds are decoupled from RootBudget.cycles_used.
    assert_eq!(world.db.get_budget("t1").unwrap().cycles_used, 0);

    let note = world
        .task("t1")
        .context_notes
        .into_iter()
        .find(|n| n.starts_with("audit_rework:"))
        .unwrap();
    assert!(note.contains("ASSERT"));
    assert!(note.contains("crates/a/src/lib.rs:42-48"));
    assert!(note.contains("STUB"));
    assert!(note.contains("foo.rs:12:3"));
    assert!(!note.contains("HARDCODED"));
    assert!(!note.contains("AC-1"));

    let events = world.db.events_for_task("t1", 50).unwrap();
    let dropped_ev = events
        .iter()
        .find(|e| {
            e.kind == EventKind::DetectorHit && e.payload_json.contains("audit_rework_filter")
        })
        .unwrap();
    let payload: serde_json::Value = serde_json::from_str(&dropped_ev.payload_json).unwrap();
    assert_eq!(payload["dropped_count"], 2);
    assert_eq!(payload["dropped"][0]["id"], "HARDCODED");
    assert_eq!(payload["dropped"][0]["reason"], "empty_evidence");
    assert_eq!(payload["dropped"][1]["id"], "AC-1");
    assert_eq!(payload["dropped"][1]["reason"], "missing_file_line");
}

#[test]
fn score_findings_g4_and_score_gate_pass_without_file_line_unless_empty() {
    let world = World::new();
    let task = add_gating_task(&world, "t1");
    let deps = world.deps();

    let findings = vec![
        finding(
            "G4",
            "score 82/100 (min criterion 90)",
            "missing boundary regression test",
        ),
        finding(
            "score_gate",
            "effective score 91/100 (min average 95)",
            "average below threshold",
        ),
        finding("G2", "score 85/100 (min criterion 90)", ""),
    ];
    let next = handle_audit_failure(&deps, &task, &findings).unwrap();

    assert!(next.is_some());
    let note = world
        .task("t1")
        .context_notes
        .into_iter()
        .find(|n| n.starts_with("audit_rework:"))
        .unwrap();
    assert!(note.contains("G4:"));
    assert!(note.contains("missing boundary regression test"));
    assert!(note.contains("score_gate:"));
    assert!(note.contains("average below threshold"));
    assert!(!note.contains("G2:"));
}

#[test]
fn collect_rework_findings_skips_passing_minor_nits_when_average_meets_threshold() {
    let cfg = CrossAuditConfig {
        score_gate: true,
        min_average_score: 95,
        min_criterion_score: 90,
        ..CrossAuditConfig::default()
    };
    let scores = vec![
        GoldenScore {
            id: "G1".to_string(),
            score: 98,
            deduction_reason: "minor nit".to_string(),
        },
        GoldenScore {
            id: "G4".to_string(),
            score: 82,
            deduction_reason: "missing boundary test".to_string(),
        },
        GoldenScore {
            id: "G6".to_string(),
            score: 96,
            deduction_reason: "tiny style note".to_string(),
        },
    ];
    let verdict = AuditVerdict::Pass {
        warnings: Vec::new(),
    };
    let reasons = vec!["G4 scored 82 < 90".to_string()];

    let kept = collect_rework_findings(&verdict, &scores, &reasons, 95, &cfg);
    let ids: Vec<&str> = kept.iter().map(|f| f.id.as_str()).collect();
    assert_eq!(ids, vec!["G4"]);

    let low_avg = collect_rework_findings(&verdict, &scores, &reasons, 92, &cfg);
    let low_ids: Vec<&str> = low_avg.iter().map(|f| f.id.as_str()).collect();
    assert_eq!(low_ids, vec!["G4", "G6", "G1"]);
}

#[test]
fn dropped_findings_event_does_not_shadow_cross_audit_in_handover() {
    let world = World::new();
    let task = add_gating_task(&world, "t1");
    let deps = world.deps();

    let post_gate_payload = json!({
        "phase": "post_gate",
        "verdict": "fail: G4 scored 82 < 90",
        "provider": "gemini",
        "reviewer": "cross-audit:gemini/default",
        "same_account": false,
        "mechanical_score": 100,
        "llm_average": 93,
        "effective_score": 93,
        "scores": [{ "id": "G4", "score": 82, "deduction_reason": "missing boundary test" }],
        "tree_fp": "git-tree:t1",
        "config_hash": "cfg-hash-1",
    });
    world
        .db
        .emit(
            &FactoryEvent::new(EventKind::CrossAudit, post_gate_payload.to_string(), 900)
                .for_task("t1"),
        )
        .unwrap();

    // Emit dropped-findings audit event at a newer timestamp (1000).
    let findings = vec![finding("STUB", "stub?", "no location")];
    assert!(
        handle_audit_failure(&deps, &task, &findings)
            .unwrap()
            .is_none()
    );

    let events = world.db.events_for_task("t1", 50).unwrap();
    let handover = build_handover_from_parts(
        &world.task("t1"),
        &events,
        None,
        None,
        None,
        false,
        true,
        95,
        90,
        false,
    );
    assert_eq!(handover.mechanical_score, Some(100));
    assert_eq!(handover.llm_average, Some(93));
    assert_eq!(handover.effective_score, Some(93));
    assert_eq!(handover.scores.len(), 1);
    assert_eq!(handover.scores[0].id, "G4");
    assert_eq!(handover.tree_fp.as_deref(), Some("git-tree:t1"));
}

#[test]
fn has_file_line_ref_handles_trailing_punctuation_root_files_and_rejects_hosts() {
    for good in [
        "(src/lib.rs:42).",
        "`src/lib.rs:42`.",
        "(see src/lib.rs:42).",
        "Makefile:10",
        "Dockerfile:5",
        "Justfile:12",
        "BUILD:8",
        "Cargo.toml:3",
        ".gitignore:2",
        "a.c:1",
    ] {
        assert!(has_file_line_ref(good), "expected valid ref: {good}");
    }
    for bad in [
        "example.com:443",
        "localhost.localdomain:8080",
        "https://example.com:443/foo",
        "v1.2:3",
        "12:30",
        "note:1",
        "src/lib.rs:0",
    ] {
        assert!(!has_file_line_ref(bad), "expected rejected ref: {bad}");
    }
}

#[test]
fn fallback_rejection_note_never_leaks_unverified_findings() {
    let only_invalid = AuditVerdict::Fail {
        findings: vec![
            finding("SECRET", "added secret?", ""),
            finding("STUB", "stubbed?", "hallucinated prose without location"),
        ],
        warnings: Vec::new(),
    };
    let summary = fallback_verdict_summary(&only_invalid, "fail: SECRET; STUB");
    assert_eq!(summary, "unverified findings dropped");
    assert!(!summary.contains("SECRET"));
    assert!(!summary.contains("hallucinated prose"));

    let mixed = AuditVerdict::Fail {
        findings: vec![
            finding("ASSERT", "weakened?", "src/lib.rs:15 removed assert"),
            finding("STUB", "stubbed?", "no location"),
        ],
        warnings: Vec::new(),
    };
    let mixed_summary = fallback_verdict_summary(&mixed, "raw");
    assert!(mixed_summary.contains("src/lib.rs:15"));
    assert!(!mixed_summary.contains("STUB"));

    // End-to-end: locked file audit fails with ONLY an unverified finding ->
    // fallback context note must NOT leak the raw finding into context_notes.
    let world = World::new();
    world.add("t1");
    world
        .gate
        .state
        .lock()
        .unwrap()
        .locked
        .insert("t1".to_string(), "lockfp1".to_string());
    let mut answers: Vec<serde_json::Value> = build_checklist(&spec(), false)
        .iter()
        .map(|item| json!({ "id": item.id, "answer": "no" }))
        .collect();
    answers[0]["answer"] = json!("yes");
    answers[0]["evidence"] = json!("hallucinated prose without any path");
    world.runner.0.lock().unwrap().audit_text = Some(json!({ "answers": answers }).to_string());

    let _ = world.once();
    let task = world.task("t1");
    let notes = task.context_notes;
    // RFC-032: an unbacked "yes" leaves the audit without a verdict, which
    // parks the task as an infra block instead of sending a rework note.
    assert_eq!(
        task.state,
        aaf_domain::task::TaskState::Blocked,
        "{notes:?}"
    );
    assert!(notes.is_empty(), "{notes:?}");
    assert!(
        !notes.iter().any(|n| n.contains("hallucinated prose")),
        "raw unverified finding leaked into context_notes: {notes:?}"
    );
}

#[test]
fn score_findings_prioritize_failing_g6_g7_ahead_of_passing_g1_g2() {
    use crate::factory::unblock::format_sanitized_audit_findings;

    let long_reason = "x".repeat(600);
    let scores = vec![
        GoldenScore {
            id: "G1".into(),
            score: 92,
            deduction_reason: format!("src/a.rs:10 {long_reason}"),
        },
        GoldenScore {
            id: "G2".into(),
            score: 92,
            deduction_reason: format!("src/b.rs:20 {long_reason}"),
        },
        GoldenScore {
            id: "G3".into(),
            score: 90,
            deduction_reason: format!("src/c.rs:30 {long_reason}"),
        },
        GoldenScore {
            id: "G4".into(),
            score: 86,
            deduction_reason: "tests/t.py:55 regex test brittle".into(),
        },
        GoldenScore {
            id: "G5".into(),
            score: 96,
            deduction_reason: String::new(),
        },
        GoldenScore {
            id: "G6".into(),
            score: 72,
            deduction_reason: "tools/admin.html:2891 crammed 1000-char line".into(),
        },
        GoldenScore {
            id: "G7".into(),
            score: 80,
            deduction_reason: "tools/admin.html:2956 alert without structured log".into(),
        },
    ];
    let cfg = CrossAuditConfig {
        score_gate: true,
        min_average_score: 95,
        min_criterion_score: 90,
        ..CrossAuditConfig::default()
    };
    let findings = collect_rework_findings(
        &AuditVerdict::Pass { warnings: vec![] },
        &scores,
        &["effective score 86".into()],
        86,
        &cfg,
    );
    let ids: Vec<&str> = findings.iter().map(|f| f.id.as_str()).collect();
    assert_eq!(
        &ids[..3],
        &["G6", "G7", "G4"],
        "lowest failing scores first: {ids:?}"
    );

    let formatted = format_sanitized_audit_findings(&findings).unwrap();
    assert!(
        formatted.contains("G6:") && formatted.contains("crammed 1000-char line"),
        "{formatted}"
    );
    assert!(
        formatted.contains("G7:") && formatted.contains("structured log"),
        "{formatted}"
    );
    assert!(
        formatted.contains("G4:") && formatted.contains("regex test brittle"),
        "{formatted}"
    );
}

#[test]
fn handle_audit_failure_stops_early_on_score_plateau() {
    let world = World::new();
    let task = add_gating_task(&world, "t-plat");
    let deps = world.deps();
    let emit_score = |score: u8| {
        deps.emit(
            EventKind::CrossAudit,
            "t-plat",
            json!({
                "phase": "post_gate",
                "verdict": format!("fail: effective score {score} < 95"),
                "effective_score": score,
            }),
        )
        .unwrap();
    };
    let findings = vec![finding(
        "G6",
        "score 72/100",
        "tools/admin.html:2891 long line",
    )];

    // 1st fail (86) -> rework allowed (Gating -> Ready).
    emit_score(86);
    assert!(
        handle_audit_failure(&deps, &task, &findings)
            .unwrap()
            .is_some()
    );
    // 2nd fail (86, no gain) -> still allowed once.
    deps.move_to("t-plat", aaf_domain::task::TaskState::Running, None, "r1")
        .unwrap();
    deps.move_to("t-plat", aaf_domain::task::TaskState::Gating, None, "g1")
        .unwrap();
    emit_score(86);
    assert!(
        handle_audit_failure(&deps, &task, &findings)
            .unwrap()
            .is_some()
    );
    // 3rd fail (85, two consecutive non-improving reworks) -> plateau stops rework early.
    deps.move_to("t-plat", aaf_domain::task::TaskState::Running, None, "r2")
        .unwrap();
    deps.move_to("t-plat", aaf_domain::task::TaskState::Gating, None, "g2")
        .unwrap();
    emit_score(85);
    assert!(
        handle_audit_failure(&deps, &task, &findings)
            .unwrap()
            .is_none(),
        "plateau (86 -> 86 -> 85) must stop audit_rework early"
    );
}
