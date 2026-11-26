#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Pillar 1 (LoopForge essence): infra failure separation & review rework.

use std::path::PathBuf;

use aaf_domain::blocked::{BlockedReason, SubtaskPlan, UnblockAction, UnblockTier};
use aaf_domain::budget::RootBudget;
use aaf_domain::config::UnblockerConfig;
use aaf_domain::event::EventKind;
use aaf_domain::gate::{
    GateMode, GateStatus, HeadLockReport, LockStatus, RECEIPT_SCHEMA_VERSION, Receipt,
    StoreComplianceReport,
};
use aaf_domain::ports::{
    AgentOutcome, BudgetStore, Clock, EventSink, InfraFailure, InfraKind, LimitHit, PortError,
    PortResult, TaskStore,
};
use aaf_domain::proof::{PROOF_SCHEMA_VERSION, ProofRecord};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::executors::claude::parse_stream;
use aaf_engine::factory::prompt::{PromptContext, build_prompt_with_context};
use aaf_engine::git::{PrRequest, TaskWorktree, pr_body_with_proof};
use aaf_engine::unblocker::{FailureSignal, Ladder, SubtaskProposer, classify, failure_digest};

struct FixedClock(i64);
impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0
    }
}

fn infra_outcome(tail: &str) -> AgentOutcome {
    AgentOutcome {
        exit_code: Some(1),
        timed_out: false,
        final_text: String::new(),
        tokens_in: 0,
        tokens_out: 0,
        limit_hit: None,
        infra: Some(InfraFailure {
            kind: InfraKind::Network,
            raw: tail.to_string(),
        }),
        log_tail: tail.to_string(),
    }
}

#[test]
fn infra_outcome_is_not_success_and_classifies_as_infra() {
    let outcome = infra_outcome("503 Service Unavailable from upstream API");
    assert!(!outcome.succeeded());

    let mut signal = FailureSignal::empty();
    signal.outcome = Some(outcome);
    assert!(matches!(classify(&signal), BlockedReason::Infra { .. }));
    let first_digest = failure_digest(&signal);

    let mut second = FailureSignal::empty();
    second.outcome = Some(infra_outcome("ECONNRESET talking to api.anthropic.com"));
    assert_eq!(failure_digest(&second), first_digest);
}

#[test]
fn claude_stream_parser_flags_auth_and_network_as_infra_not_quota() {
    let stream_auth = "{\"type\":\"result\",\"result\":\"Unauthorized\",\"is_error\":true,\"api_error_status\":401,\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}\n";
    let parsed_auth = parse_stream(stream_auth);
    assert!(parsed_auth.infra.is_some());
    assert!(parsed_auth.limit_hit.is_none());

    let stream_429 = "{\"type\":\"result\",\"result\":\"rate limited\",\"is_error\":true,\"api_error_status\":429,\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}\n";
    let parsed_429 = parse_stream(stream_429);
    assert!(parsed_429.infra.is_none());
    assert!(parsed_429.limit_hit.is_some());

    let stream_ok = "{\"type\":\"result\",\"result\":\"all good\",\"is_error\":false,\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}\n";
    let parsed_ok = parse_stream(stream_ok);
    assert!(parsed_ok.infra.is_none());
}

#[test]
fn ladder_retries_infra_once_at_t2_then_escalates_skipping_t3_t4() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("state.db")).unwrap();
    let cfg = UnblockerConfig::default();
    let clock = FixedClock(100);
    let spec = ImmutableSpec {
        description: "desc".to_string(),
        acceptance: vec!["ok".to_string()],
    };
    let mut task = Task::new_root("t-1", "/repo", "task", spec, Priority::P2, 100);
    task.state = TaskState::Running;
    db.insert_task(&task).unwrap();

    let ladder = Ladder::new(&db, &db, &db, &db, &clock, &cfg);
    let mut signal = FailureSignal::empty();
    signal.outcome = Some(infra_outcome("502 Bad Gateway"));

    let first = ladder.next_action("t-1", &signal, None).unwrap();
    assert_eq!(first.tier, UnblockTier::T2Mechanical);
    assert!(matches!(first.reason, BlockedReason::Infra { .. }));
    assert!(matches!(first.action, UnblockAction::RetryInfra { .. }));

    let second = ladder.next_action("t-1", &signal, None).unwrap();
    assert_eq!(second.tier, UnblockTier::T5Escalate);
    assert!(matches!(second.reason, BlockedReason::Infra { .. }));
    assert!(matches!(second.action, UnblockAction::EscalateHuman { .. }));
}

#[test]
fn review_can_transition_directly_to_gating_in_db() {
    assert!(TaskState::Review.can_transition_to(TaskState::Gating));
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("state.db")).unwrap();
    let spec = ImmutableSpec {
        description: "desc".to_string(),
        acceptance: vec!["ok".to_string()],
    };
    let mut task = Task::new_root("t-rev", "/repo", "review task", spec, Priority::P2, 100);
    task.state = TaskState::Review;
    db.insert_task(&task).unwrap();
    db.set_state("t-rev", TaskState::Gating, None, 101).unwrap();
    assert_eq!(db.get_task("t-rev").unwrap().state, TaskState::Gating);
}

fn plain_failed_outcome(log_tail: &str, final_text: &str) -> AgentOutcome {
    AgentOutcome {
        exit_code: Some(1),
        timed_out: false,
        final_text: final_text.to_string(),
        tokens_in: 0,
        tokens_out: 0,
        limit_hit: None,
        infra: None,
        log_tail: log_tail.to_string(),
    }
}

#[test]
fn transient_http_503_529_and_overloaded_classify_as_infra_even_without_executor_flag() {
    for (label, tail) in [
        ("http 503", "Error: HTTP 503 Service Unavailable"),
        ("529 overloaded", "API returned 529 Overloaded"),
        ("overloaded_error json", "{\"type\":\"overloaded_error\"}"),
        ("api_error json", "{\"type\": \"api_error\"}"),
        ("status 502", "request failed with status 502"),
    ] {
        let mut signal = FailureSignal::empty();
        signal.outcome = Some(plain_failed_outcome(tail, ""));
        assert!(
            matches!(classify(&signal), BlockedReason::Infra { .. }),
            "case {label} should classify as Infra"
        );
        let mut explicit = FailureSignal::empty();
        explicit.outcome = Some(infra_outcome("connection reset"));
        assert_eq!(failure_digest(&signal), failure_digest(&explicit));
    }

    // Agent prose (final_text) and bare words in log_tail must NOT trigger heuristic Infra.
    for (tail, final_text) in [
        ("", "API returned 529 Overloaded"),
        ("compiler error: overloaded method not found", ""),
        ("test failed: enum variant server_error mismatch", ""),
    ] {
        let mut sig = FailureSignal::empty();
        sig.outcome = Some(plain_failed_outcome(tail, final_text));
        assert!(!matches!(classify(&sig), BlockedReason::Infra { .. }));
    }

    // Merge conflicts and timeouts take precedence over heuristic transient_infra_detail.
    let mut conflict = FailureSignal::empty();
    conflict.outcome = Some(plain_failed_outcome(
        "Error: HTTP 503 Service Unavailable",
        "",
    ));
    conflict.merge_conflict_files = vec!["src/lib.rs".to_string()];
    assert!(matches!(
        classify(&conflict),
        BlockedReason::MergeConflict { .. }
    ));

    let mut timeout = FailureSignal::empty();
    let mut timed = plain_failed_outcome("Error: HTTP 503 Service Unavailable", "");
    timed.timed_out = true;
    timeout.outcome = Some(timed);
    assert!(matches!(classify(&timeout), BlockedReason::Stuck { .. }));

    // A successful run whose text merely mentions "overloaded" is NOT infra.
    let mut ok = FailureSignal::empty();
    ok.outcome = Some(AgentOutcome {
        exit_code: Some(0),
        log_tail: "fixed overloaded method".to_string(),
        ..plain_failed_outcome("", "")
    });
    assert!(!matches!(classify(&ok), BlockedReason::Infra { .. }));
}

#[test]
fn infra_and_quota_do_not_burn_unblock_cycles_or_freeze_root_budget() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("state.db")).unwrap();
    let cfg = UnblockerConfig {
        max_unblock_cycles: 1,
        ..UnblockerConfig::default()
    };
    let clock = FixedClock(500);
    let spec = ImmutableSpec {
        description: "desc".to_string(),
        acceptance: vec!["ok".to_string()],
    };
    let mut task = Task::new_root("t-iq", "/repo", "task", spec, Priority::P2, 100);
    task.state = TaskState::Running;
    db.insert_task(&task).unwrap();
    // Already at cycles_limit (1/1): any real cycle charge would trigger FreezeRoot.
    let mut budget = RootBudget::new("t-iq", &cfg);
    budget.cycles_used = 1;
    db.put_budget(&budget).unwrap();

    let ladder = Ladder::new(&db, &db, &db, &db, &clock, &cfg);

    let mut quota = FailureSignal::empty();
    quota.outcome = Some(AgentOutcome {
        limit_hit: Some(LimitHit {
            reset_at: Some(900),
            raw: "rate limit".to_string(),
        }),
        ..plain_failed_outcome("429", "")
    });
    for _ in 0..3 {
        let dec = ladder.next_action("t-iq", &quota, None).unwrap();
        assert!(matches!(dec.reason, BlockedReason::Quota { .. }));
        assert!(!matches!(dec.action, UnblockAction::FreezeRoot { .. }));
    }
    assert_eq!(db.get_task("t-iq").unwrap().unblock_cycles, 0);
    assert_eq!(db.get_budget("t-iq").unwrap().cycles_used, 1);
    assert!(!db.get_budget("t-iq").unwrap().frozen);

    let mut infra = FailureSignal::empty();
    infra.outcome = Some(plain_failed_outcome("HTTP 529 overloaded", ""));
    let dec1 = ladder.next_action("t-iq", &infra, None).unwrap();
    assert!(matches!(dec1.action, UnblockAction::RetryInfra { .. }));
    let dec2 = ladder.next_action("t-iq", &infra, None).unwrap();
    assert!(matches!(dec2.action, UnblockAction::EscalateHuman { .. }));
    assert_eq!(db.get_task("t-iq").unwrap().unblock_cycles, 0);
    assert_eq!(db.get_budget("t-iq").unwrap().cycles_used, 1);
    assert!(!db.get_budget("t-iq").unwrap().frozen);
}

#[test]
fn ladder_tried_resets_after_human_rework_note() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("state.db")).unwrap();
    let cfg = UnblockerConfig {
        max_unblock_cycles: 20,
        ..UnblockerConfig::default()
    };
    let clock = FixedClock(200);
    let spec = ImmutableSpec {
        description: "desc".to_string(),
        acceptance: vec!["ok".to_string()],
    };
    let mut task = Task::new_root("t-rw", "/repo", "rework task", spec, Priority::P2, 100);
    task.state = TaskState::Running;
    db.insert_task(&task).unwrap();

    let ladder = Ladder::new(&db, &db, &db, &db, &clock, &cfg);
    let mut sig = FailureSignal::empty();
    sig.outcome = Some(plain_failed_outcome("compile error E0425", ""));

    // First attempt: T3FreshContext; second attempt (same digest): T5Escalate (no proposer).
    let d1 = ladder.next_action("t-rw", &sig, None).unwrap();
    assert_eq!(d1.tier, UnblockTier::T3FreshContext);
    let d2 = ladder.next_action("t-rw", &sig, None).unwrap();
    assert_eq!(d2.tier, UnblockTier::T5Escalate);

    // Human rework note resets the tried-tier scan.
    db.append_context_note("t-rw", "rework: fix lint and retest", 205)
        .unwrap();
    let d3 = ladder.next_action("t-rw", &sig, None).unwrap();
    assert_eq!(d3.tier, UnblockTier::T3FreshContext);
}

#[test]
fn pr_body_with_proof_sanitizes_markdown_and_prompt_includes_repo_env() {
    let tmp = tempfile::tempdir().unwrap();
    let skill_dir = tmp.path().join(".claude/skills/android-debug");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(skill_dir.join("SKILL.md"), "# skill\n").unwrap();

    let spec = ImmutableSpec {
        description: "desc".to_string(),
        acceptance: vec!["ok".to_string()],
    };
    let task = Task::new_root("t-env", "/repo", "env task", spec, Priority::P2, 1);
    let prompt = build_prompt_with_context(
        &task,
        "aaf/t-env",
        &PromptContext {
            repo_root: tmp.path(),
            profile: "android",
            proof_required: true,
            repo_memory: None,
        },
    );
    assert!(prompt.contains("## Repo environment"), "{prompt}");
    assert!(prompt.contains("`android`"), "{prompt}");
    assert!(prompt.contains("android-debug"), "{prompt}");
    assert!(prompt.contains("mobile_save_screenshot"), "{prompt}");
    assert!(prompt.contains("aaf slot -- <cmd>"), "{prompt}");

    let req = PrRequest {
        repo_root: PathBuf::from("/repo"),
        worktree: TaskWorktree {
            path: PathBuf::from("/repo"),
            branch: "aaf/t-env".to_string(),
        },
        base: "main".to_string(),
        remote: "origin".to_string(),
        title: "t".to_string(),
        receipt: Receipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            repo_root: "/repo".to_string(),
            tree_fp: "git-tree:123".to_string(),
            head_sha: None,
            base_sha: None,
            mode: GateMode::Scoped,
            status: GateStatus::Clear,
            reasons: vec![],
            config_hash: "cfg".to_string(),
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
            created_at: 0,
        },
    };
    let proof = ProofRecord {
        schema_version: PROOF_SCHEMA_VERSION,
        repo_root: "/repo".to_string(),
        tree_fp: "git-tree:123".to_string(),
        image_path: "/tmp/a.png".to_string(),
        image_sha256: "abc".to_string(),
        width: 1080,
        height: 1920,
        byte_len: 10000,
        source_tool: "mobile-mcp`evil\n## Injected".to_string(),
        device_serial: "emu`1\r\nbad".to_string(),
        task_id: Some("t-env".to_string()),
        recorded_at: 1,
        base_sha: None,
        flows: Vec::new(),
    };
    let body = pr_body_with_proof(&req, Some(&proof));
    assert!(!body.contains("\n## Injected"), "{body}");
    assert!(
        body.contains("- Tool: `mobile-mcp'evil ## Injected`\n"),
        "{body}"
    );
    assert!(body.contains("- Device: `emu'1  bad`\n"), "{body}");
}

struct FailingProposer(&'static str);
impl SubtaskProposer for FailingProposer {
    fn propose(&self, _: &Task, _: &BlockedReason, _: &str) -> PortResult<Vec<SubtaskPlan>> {
        Err(PortError::Io(self.0.to_string()))
    }
}

#[test]
fn t4_proposer_infra_or_limit_hit_charges_no_cycle_and_emits_no_t5_escalate() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("state.db")).unwrap();
    let cfg = UnblockerConfig::default();
    let clock = FixedClock(300);
    let spec = ImmutableSpec {
        description: "desc".to_string(),
        acceptance: vec!["ok".to_string()],
    };
    let mut task = Task::new_root("t-prop", "/repo", "proposer task", spec, Priority::P2, 100);
    task.state = TaskState::Running;
    db.insert_task(&task).unwrap();

    let ladder = Ladder::new(&db, &db, &db, &db, &clock, &cfg);
    let mut sig = FailureSignal::empty();
    sig.outcome = Some(plain_failed_outcome("compile error E0425", ""));

    // Step 1 consumes T3 -> 1 cycle used.
    let d1 = ladder.next_action("t-prop", &sig, None).unwrap();
    assert_eq!(d1.tier, UnblockTier::T3FreshContext);
    assert_eq!(db.get_task("t-prop").unwrap().unblock_cycles, 1);
    assert_eq!(db.get_budget("t-prop").unwrap().cycles_used, 1);

    // Step 2 reaches T4 proposer; infra: and limit_hit: must NOT burn cycles or emit T5Escalate.
    let infra_dec = ladder
        .next_action("t-prop", &sig, Some(&FailingProposer("infra: 503 down")))
        .unwrap();
    assert_ne!(infra_dec.tier, UnblockTier::T5Escalate);
    assert!(matches!(infra_dec.action, UnblockAction::RetryInfra { .. }));
    assert_eq!(db.get_task("t-prop").unwrap().unblock_cycles, 1);
    assert_eq!(db.get_budget("t-prop").unwrap().cycles_used, 1);

    let limit_dec = ladder
        .next_action("t-prop", &sig, Some(&FailingProposer("limit_hit: 429")))
        .unwrap();
    assert_ne!(limit_dec.tier, UnblockTier::T5Escalate);
    assert!(matches!(limit_dec.reason, BlockedReason::Quota { .. }));
    assert_eq!(db.get_task("t-prop").unwrap().unblock_cycles, 1);
    assert_eq!(db.get_budget("t-prop").unwrap().cycles_used, 1);

    let events = db.events_for_task("t-prop", 50).unwrap();
    assert!(
        !events
            .iter()
            .any(|e| e.kind == EventKind::UnblockStep && e.payload_json.contains("T5")),
        "{events:?}"
    );
}
