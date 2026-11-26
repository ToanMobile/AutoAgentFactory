#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Wave 2 CLI tests: `aaf board approve-plan`, `aaf board unblock` on stalled
//! plan debates, and compact `aaf board show` / `aaf board trace` rendering.

#[path = "factory_support.rs"]
mod support;

use std::process::Command;

use aaf_domain::blocked::BlockedReason;
use aaf_domain::budget::RootBudget;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::pipeline::{
    PlanChecklistItem, PlanContract, PlanCritique, PlanCritiqueVerdict, PlanFileChange,
    RedGreenTestPlan,
};
use aaf_domain::ports::{BudgetStore, EventSink, TaskStore};
use aaf_domain::task::TaskState;
use aaf_engine::db::Db;
use aaf_engine::plan_debate::{
    PLAN_DEBATE_STALLED_PREFIX, canonical_plan_hash, extract_plan_contract_hash,
    replay_debate_state,
};
use serde_json::{Value, json};
use support::Fixture;

fn aaf_text(fx: &Fixture, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_aaf"))
        .args(args)
        .current_dir(&fx.repo)
        .env_clear()
        .env("AAF_NO_BUILTIN_DEFAULTS", "1")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", fx.root().join("home"))
        .env("AAF_DATA_DIR", &fx.data)
        .env("AAF_WORKTREE_DIR", &fx.worktrees)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "aaf {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn emit(db: &Db, id: &str, kind: EventKind, payload: Value, ts: i64) {
    db.emit(&FactoryEvent::new(kind, payload.to_string(), ts).for_task(id))
        .unwrap();
}

fn block_human(db: &Db, id: &str, why: String, ts: i64) {
    let reason = BlockedReason::NeedsHuman { why };
    db.set_state(id, TaskState::Blocked, Some(&reason), ts)
        .unwrap();
}

fn parse_plan(v: &Value) -> PlanContract {
    serde_json::from_value(v.clone()).unwrap()
}

fn bulky_plan(summary: &str, sentinel: &str, pad: usize) -> (PlanContract, String) {
    let change = PlanFileChange {
        path: "src/cache.rs".into(),
        responsibility: "cache".into(),
        risk: "low".into(),
    };
    let test = RedGreenTestPlan {
        ac_ref: "AC-1".into(),
        test_path: "tests/cache_test.rs".into(),
        red_condition: "miss panics".into(),
        green_condition: "returns entry".into(),
    };
    let plan = PlanContract {
        summary: summary.into(),
        tier_assessment: "M".into(),
        tier_reasons: vec!["multi-file".into()],
        files_to_change: vec![change],
        invariants_and_edge_cases: vec![format!("{sentinel}_{}", "x".repeat(pad))],
        red_green_tests: vec![test],
        dispositions: vec![],
        open_questions: vec![],
    };
    let hash = canonical_plan_hash(&plan);
    (plan, hash)
}

fn author_payload(round: u32, plan: &PlanContract, hash: &str) -> Value {
    json!({
        "round": round, "role": "author", "ok": true,
        "summary": plan.summary, "plan_contract_hash": hash, "plan_contract": plan
    })
}

#[test]
fn approve_plan_on_stalled_debate_emits_human_converged_resets_budget_and_appends_compact_note() {
    let fx = Fixture::new();
    let id = fx.add("Approve stalled debate");
    let (plan, hash) = bulky_plan("Add bounded cache", "BULKY_CONTRACT_SENTINEL_1111", 8_192);
    {
        let db = fx.db();
        emit(
            &db,
            &id,
            EventKind::PlanDebateTurn,
            author_payload(2, &plan, &hash),
            1_000,
        );
        let budget = RootBudget {
            root_id: id.clone(),
            token_limit: 100_000,
            tokens_used: 250_000,
            wall_secs_limit: 600,
            wall_secs_used: 700,
            cycles_limit: 3,
            cycles_used: 4,
            frozen: true,
        };
        db.put_budget(&budget).unwrap();
        let why = format!("{PLAN_DEBATE_STALLED_PREFIX} round 2 reached max_plan_rounds");
        block_human(&db, &id, why, 1_001);
    }

    let out = fx.aaf(&["board", "approve-plan", &id]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(
        (
            out.json["ok"].as_bool(),
            out.json["approved"].as_bool(),
            out.json["state"].as_str()
        ),
        (Some(true), Some(true), Some("ready"))
    );

    let db = fx.db();
    let task = db.get_task(&id).unwrap();
    assert_eq!(task.state, TaskState::Ready);
    assert!(task.blocked_reason.is_none());

    let note = task
        .context_notes
        .iter()
        .find(|n| n.starts_with("plan_approved:"))
        .unwrap();
    assert!(
        note.contains("Add bounded cache")
            && note.contains(&format!("[plan_contract_hash={hash}]"))
    );
    assert!(!note.contains("BULKY_CONTRACT_SENTINEL_1111") && note.len() < 512);
    assert_eq!(
        extract_plan_contract_hash(&task.context_notes),
        Some(hash.clone())
    );

    let b = db.get_budget(&id).unwrap();
    assert_eq!(
        (b.tokens_used, b.wall_secs_used, b.cycles_used, b.frozen),
        (0, 0, 0, false)
    );

    let conv = db
        .events_for_task_by_kinds(&id, &[EventKind::PlanConverged], 10)
        .unwrap();
    assert_eq!(conv.len(), 1);
    let payload: Value = serde_json::from_str(&conv[0].payload_json).unwrap();
    assert_eq!(payload["forced_by"], json!("human"));
    assert_eq!(payload["plan_contract_hash"], json!(hash));
    assert_eq!(parse_plan(&payload["plan_contract"]), plan);
}

#[test]
fn unblock_on_stalled_debate_does_not_auto_approve_plan_unlike_plan_approval() {
    let fx = Fixture::new();
    let stalled_id = fx.add("Stalled debate task");
    let approval_id = fx.add("Approval gated task");
    let (plan, hash) = bulky_plan("Shared plan summary", "SENTINEL_2222", 256);
    {
        let db = fx.db();
        emit(
            &db,
            &stalled_id,
            EventKind::PlanDebateTurn,
            author_payload(2, &plan, &hash),
            1_000,
        );
        block_human(
            &db,
            &stalled_id,
            format!("{PLAN_DEBATE_STALLED_PREFIX} repeated P1"),
            1_001,
        );
        let generated =
            json!({"summary": plan.summary, "plan_contract_hash": hash, "plan_contract": plan});
        emit(
            &db,
            &approval_id,
            EventKind::PlanGenerated,
            generated,
            1_002,
        );
        block_human(
            &db,
            &approval_id,
            "plan approval required: Shared plan summary".into(),
            1_003,
        );
    }

    let u1 = fx.aaf(&["board", "unblock", &stalled_id]);
    assert_eq!(u1.code, 0, "{}", u1.stderr);
    assert_eq!(
        (
            u1.json["state"].as_str(),
            u1.json["plan_approved"].as_bool()
        ),
        (Some("ready"), Some(false))
    );

    let u2 = fx.aaf(&["board", "unblock", &approval_id]);
    assert_eq!(u2.code, 0, "{}", u2.stderr);
    assert_eq!(
        (
            u2.json["state"].as_str(),
            u2.json["plan_approved"].as_bool()
        ),
        (Some("ready"), Some(true))
    );

    let db = fx.db();
    let stalled = db.get_task(&stalled_id).unwrap();
    assert_eq!(stalled.state, TaskState::Ready);
    assert!(
        !stalled
            .context_notes
            .iter()
            .any(|n| n.starts_with("plan_approved:"))
    );
    assert!(
        db.events_for_task_by_kinds(&stalled_id, &[EventKind::PlanConverged], 10)
            .unwrap()
            .is_empty()
    );

    let approved = db.get_task(&approval_id).unwrap();
    assert_eq!(approved.state, TaskState::Ready);
    assert!(
        approved
            .context_notes
            .iter()
            .any(|n| n.starts_with("plan_approved:") && n.contains(&hash))
    );
    assert_eq!(
        db.events_for_task_by_kinds(&approval_id, &[EventKind::PlanConverged], 10)
            .unwrap()
            .len(),
        1
    );
    let replay_kinds = [
        EventKind::PlanDebateTurn,
        EventKind::PlanConverged,
        EventKind::TaskStateChanged,
        EventKind::ReworkRequested,
    ];
    let approval_events = db
        .events_for_task_by_kinds(&approval_id, &replay_kinds, 50)
        .unwrap();
    let approval_snap = replay_debate_state(&approval_events);
    assert!(
        approval_snap.converged.is_some(),
        "replay_debate_state must retain converged plan after `aaf board unblock <approval_id>`"
    );
    let stalled_events = db
        .events_for_task_by_kinds(&stalled_id, &replay_kinds, 50)
        .unwrap();
    let stalled_snap = replay_debate_state(&stalled_events);
    assert!(
        stalled_snap.converged.is_none() && stalled_snap.round == 0,
        "replay_debate_state must reset epoch after `aaf board unblock <stalled_id>`"
    );
}

#[test]
fn board_show_and_trace_render_compact_text_while_json_preserves_full_payload() {
    let fx = Fixture::new();
    let id = fx.add("Compact display check");
    let (plan, hash) = bulky_plan(
        "Implement cache layer",
        "RAW_PLAN_JSON_SENTINEL_9999",
        10_240,
    );
    let item = PlanChecklistItem {
        id: "PLAN-SCOPE".into(),
        pass: true,
        evidence: format!("RAW_CRITIQUE_SENTINEL_8888_{}", "y".repeat(4_096)),
    };
    let critique = PlanCritique {
        checklist: vec![item],
        resolved_from_previous: vec![],
        issues: vec![],
        verdict: PlanCritiqueVerdict::Converged,
    };
    {
        let db = fx.db();
        emit(
            &db,
            &id,
            EventKind::PlanDebateTurn,
            author_payload(1, &plan, &hash),
            2_000,
        );
        let chal = json!({"round":1,"role":"challenger","ok":true,"verdict":"CONVERGED","open_count":0,"critique":critique});
        emit(&db, &id, EventKind::PlanDebateTurn, chal, 2_001);
        let conv = json!({"summary":plan.summary,"plan_contract_hash":hash,"plan_contract":plan,"forced_by":"human"});
        emit(&db, &id, EventKind::PlanConverged, conv, 2_002);
    }

    let conv_line = format!(
        "plan_converged: Implement cache layer hash={} forced_by=human",
        &hash[..12]
    );
    let show_text = aaf_text(&fx, &["board", "show", &id]);
    assert!(
        show_text.contains("plan_debate_turn: r1 author ok=true"),
        "{show_text}"
    );
    assert!(show_text.contains("plan_debate_turn: r1 challenger ok=true verdict=CONVERGED open=0"));
    assert!(show_text.contains(&conv_line), "{show_text}");
    assert!(
        !show_text.contains("RAW_PLAN_JSON_SENTINEL_9999")
            && !show_text.contains("RAW_CRITIQUE_SENTINEL_8888")
    );
    for line in show_text.lines().filter(|l| l.starts_with("  event ")) {
        assert!(line.len() <= 300, "event line len={}: {line}", line.len());
    }

    let trace_text = aaf_text(&fx, &["board", "trace", &id]);
    assert!(
        trace_text.contains("plan_debate_turn: r1 author ok=true")
            && trace_text.contains(&conv_line)
    );
    assert!(
        !trace_text.contains("RAW_PLAN_JSON_SENTINEL_9999")
            && !trace_text.contains("RAW_CRITIQUE_SENTINEL_8888")
    );
    for line in trace_text.lines().filter(|l| l.contains("[event]")) {
        assert!(line.len() <= 320, "trace line len={}: {line}", line.len());
    }

    let show_json = fx.aaf(&["board", "show", &id]);
    assert_eq!(show_json.code, 0, "{}", show_json.stderr);
    let show_evs = show_json.json["events"].as_array().unwrap();
    let show_conv = show_evs
        .iter()
        .find(|e| e["kind"] == json!("plan_converged"))
        .unwrap();
    assert_eq!(parse_plan(&show_conv["payload"]["plan_contract"]), plan);

    let trace_json = fx.aaf(&["board", "trace", &id]);
    assert_eq!(trace_json.code, 0, "{}", trace_json.stderr);
    let entries = trace_json.json["trace"]["entries"].as_array().unwrap();
    let trace_conv = entries
        .iter()
        .find(|e| e["kind"] == json!("plan_converged"))
        .unwrap();
    assert!(
        !trace_conv["summary"]
            .as_str()
            .unwrap()
            .contains("RAW_PLAN_JSON_SENTINEL_9999")
    );
    assert_eq!(parse_plan(&trace_conv["payload"]["plan_contract"]), plan);
}
