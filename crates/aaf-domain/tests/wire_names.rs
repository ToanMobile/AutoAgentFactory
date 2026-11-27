#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Serialised enum names are a storage format (SQLite rows, receipts,
//! config files). Each literal below is on disk somewhere: changing one is
//! a migration, not a rename.

use aaf_domain::account::{AccountStatus, ProviderKind};
use aaf_domain::blocked::{BlockedReason, SubtaskPlan, UnblockAction, UnblockTier};
use aaf_domain::budget::BudgetCheck;
use aaf_domain::config::StoreMode;
use aaf_domain::event::EventKind;
use aaf_domain::gate::{
    ChangeKind, GateMode, GateStatus, LockStatus, RECEIPT_SCHEMA_VERSION, Severity, StoreStatus,
};
use aaf_domain::run::RunStatus;
use aaf_domain::task::{Priority, TaskState};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

/// Each value serialises to exactly the given JSON and reads back equal.
fn assert_wire<T>(cases: &[(T, Value)])
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    for (value, wire) in cases {
        assert_eq!(&serde_json::to_value(value).unwrap(), wire, "{value:?}");
        let back: T = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(&back, value, "{wire}");
    }
}

fn rejects<T: DeserializeOwned>(wire: Value) -> bool {
    serde_json::from_value::<T>(wire).is_err()
}

#[test]
fn task_state() {
    let cases = [
        (TaskState::Backlog, "backlog"),
        (TaskState::Ready, "ready"),
        (TaskState::Running, "running"),
        (TaskState::Gating, "gating"),
        (TaskState::Review, "review"),
        (TaskState::Blocked, "blocked"),
        (TaskState::Done, "done"),
        (TaskState::Cancelled, "cancelled"),
    ];
    assert_eq!(cases.len(), TaskState::ALL.len());
    for (state, name) in cases {
        assert_wire(&[(state, json!(name))]);
        assert_eq!(state.as_str(), name);
        assert_eq!(TaskState::parse(name), Some(state));
    }
    for bad in ["", "Backlog", "DONE", " done", "canceled", "unknown"] {
        assert_eq!(TaskState::parse(bad), None, "{bad:?}");
    }
    assert!(rejects::<TaskState>(json!("Backlog")));
}

#[test]
fn priority() {
    let cases = [
        (Priority::P0, "P0"),
        (Priority::P1, "P1"),
        (Priority::P2, "P2"),
        (Priority::P3, "P3"),
    ];
    for (priority, name) in cases {
        assert_wire(&[(priority, json!(name))]);
        assert_eq!(priority.as_str(), name);
        assert_eq!(Priority::parse(name), Some(priority));
        // `parse` is for human input and ignores case; the wire form does not.
        assert_eq!(Priority::parse(&name.to_ascii_lowercase()), Some(priority));
    }
    for bad in ["", "P4", "P", "0", "P00", " P0"] {
        assert_eq!(Priority::parse(bad), None, "{bad:?}");
    }
    assert!(rejects::<Priority>(json!("p0")));
}

#[test]
fn provider_kind() {
    let cases = [
        (ProviderKind::ClaudeCode, "claude_code"),
        (ProviderKind::Codex, "codex"),
        (ProviderKind::Antigravity, "antigravity"),
        (ProviderKind::OpenCode, "open_code"),
        (ProviderKind::Grok, "grok"),
    ];
    assert_eq!(cases.len(), ProviderKind::ALL.len());
    for (provider, name) in cases {
        assert_wire(&[(provider, json!(name))]);
        assert_eq!(provider.as_str(), name);
        assert_eq!(ProviderKind::parse(name), Some(provider));
    }
    for bad in ["", "claude", "opencode", "Codex"] {
        assert_eq!(ProviderKind::parse(bad), None, "{bad:?}");
    }
}

#[test]
fn run_status() {
    let cases = [
        (RunStatus::Running, "running"),
        (RunStatus::Succeeded, "succeeded"),
        (RunStatus::Failed, "failed"),
        (RunStatus::TimedOut, "timed_out"),
        (RunStatus::LimitHit, "limit_hit"),
        (RunStatus::Killed, "killed"),
    ];
    assert_eq!(cases.len(), RunStatus::ALL.len());
    for (status, name) in cases {
        assert_wire(&[(status, json!(name))]);
        assert_eq!(status.as_str(), name);
        assert_eq!(RunStatus::parse(name), Some(status));
    }
    for bad in ["", "timedout", "Running", "success"] {
        assert_eq!(RunStatus::parse(bad), None, "{bad:?}");
    }
}

#[test]
fn event_kind() {
    let cases = [
        (EventKind::TaskCreated, "task_created"),
        (EventKind::TaskStateChanged, "task_state_changed"),
        (EventKind::RunStarted, "run_started"),
        (EventKind::RunFinished, "run_finished"),
        (EventKind::GateResult, "gate_result"),
        (EventKind::AccountCooldown, "account_cooldown"),
        (EventKind::UnblockStep, "unblock_step"),
        (EventKind::BudgetFrozen, "budget_frozen"),
        (EventKind::FactoryFrozen, "factory_frozen"),
        (EventKind::FactoryUnfrozen, "factory_unfrozen"),
        (EventKind::MergeResult, "merge_result"),
        (EventKind::PrOpened, "pr_opened"),
        (EventKind::DetectorHit, "detector_hit"),
        (EventKind::CrossAudit, "cross_audit"),
        (EventKind::WebhookSent, "webhook_sent"),
        (EventKind::ReworkRequested, "rework_requested"),
        (EventKind::ProofRecorded, "proof_recorded"),
        (EventKind::PlanGenerated, "plan_generated"),
        (EventKind::PlanDebateTurn, "plan_debate_turn"),
        (EventKind::PlanConverged, "plan_converged"),
        (EventKind::DualReviewTurn, "dual_review_turn"),
        (EventKind::ImplementerEscalated, "implementer_escalated"),
        (EventKind::FactoryHalted, "factory_halted"),
        (EventKind::PrFeedback, "pr_feedback"),
        (EventKind::CostPaused, "cost_paused"),
        (EventKind::CostResumed, "cost_resumed"),
    ];
    assert_eq!(cases.len(), EventKind::ALL.len());
    for (kind, name) in cases {
        assert_wire(&[(kind, json!(name))]);
        assert_eq!(kind.as_str(), name);
        assert_eq!(EventKind::parse(name), Some(kind));
    }
    for bad in ["", "TaskCreated", "task-created", "pr_open"] {
        assert_eq!(EventKind::parse(bad), None, "{bad:?}");
    }
}

#[test]
fn all_lists_have_no_duplicates() {
    fn distinct<T: PartialEq>(all: &[T]) -> bool {
        all.iter().enumerate().all(|(i, a)| !all[..i].contains(a))
    }
    assert!(distinct(&TaskState::ALL));
    assert!(distinct(&ProviderKind::ALL));
    assert!(distinct(&RunStatus::ALL));
    assert!(distinct(&EventKind::ALL));
}

#[test]
fn store_mode() {
    let cases = [
        (StoreMode::Off, "off"),
        (StoreMode::Auto, "auto"),
        (StoreMode::Always, "always"),
    ];
    for (mode, name) in cases {
        assert_wire(&[(mode, json!(name))]);
        assert_eq!(StoreMode::parse(name), Some(mode));
        assert_eq!(StoreMode::parse(&name.to_ascii_uppercase()), Some(mode));
    }
    for bad in ["", "on", "true", "never", " auto"] {
        assert_eq!(StoreMode::parse(bad), None, "{bad:?}");
    }
    assert!(rejects::<StoreMode>(json!("Auto")));
}

#[test]
fn budget_check() {
    assert_wire(&[
        (BudgetCheck::Ok, json!("ok")),
        (BudgetCheck::Frozen, json!("frozen")),
        (BudgetCheck::TokensExceeded, json!("tokens_exceeded")),
        (BudgetCheck::WallClockExceeded, json!("wall_clock_exceeded")),
        (BudgetCheck::CyclesExceeded, json!("cycles_exceeded")),
    ]);
}

#[test]
fn gate_enums() {
    assert_eq!(RECEIPT_SCHEMA_VERSION, 1);
    assert_wire(&[
        (Severity::Info, json!("info")),
        (Severity::Warn, json!("warn")),
        (Severity::Critical, json!("critical")),
    ]);
    assert_wire(&[
        (GateMode::Scoped, json!("scoped")),
        (GateMode::Full, json!("full")),
        (GateMode::Release, json!("release")),
    ]);
    assert_wire(&[
        (GateStatus::Clear, json!("CLEAR")),
        (GateStatus::Blocked, json!("BLOCKED")),
        (GateStatus::Unverified, json!("UNVERIFIED")),
    ]);
    assert_wire(&[
        (ChangeKind::Added, json!("added")),
        (ChangeKind::Modified, json!("modified")),
        (ChangeKind::Deleted, json!("deleted")),
        (ChangeKind::Renamed, json!("renamed")),
    ]);
    assert_wire(&[
        (LockStatus::Clear, json!("CLEAR")),
        (LockStatus::Approved, json!("APPROVED")),
        (LockStatus::Blocked, json!("BLOCKED")),
    ]);
    assert_wire(&[
        (StoreStatus::Clear, json!("CLEAR")),
        (StoreStatus::Blocked, json!("BLOCKED")),
        (StoreStatus::Skipped, json!("SKIPPED")),
        (StoreStatus::NotApplicable, json!("NOT_APPLICABLE")),
    ]);
    assert!(rejects::<GateStatus>(json!("clear")));
}

#[test]
fn account_status_is_tagged_by_status() {
    assert_wire(&[
        (AccountStatus::Available, json!({"status": "available"})),
        (
            AccountStatus::Cooldown { until: 42 },
            json!({"status": "cooldown", "until": 42}),
        ),
        (AccountStatus::Leased, json!({"status": "leased"})),
    ]);
}

#[test]
fn blocked_reason_is_tagged_by_kind_and_kind_matches_the_tag() {
    let cases = [
        (
            BlockedReason::Quota { reset_at: Some(9) },
            json!({"kind": "quota", "reset_at": 9}),
        ),
        (
            BlockedReason::Quota { reset_at: None },
            json!({"kind": "quota", "reset_at": null}),
        ),
        (
            BlockedReason::Dependency {
                task_id: "t".to_string(),
            },
            json!({"kind": "dependency", "task_id": "t"}),
        ),
        (
            BlockedReason::MergeConflict {
                detail: "d".to_string(),
            },
            json!({"kind": "merge_conflict", "detail": "d"}),
        ),
        (
            BlockedReason::FlakyTest {
                detail: "d".to_string(),
            },
            json!({"kind": "flaky_test", "detail": "d"}),
        ),
        (
            BlockedReason::Stuck {
                detail: "d".to_string(),
            },
            json!({"kind": "stuck", "detail": "d"}),
        ),
        (
            BlockedReason::GateFailed {
                summary: "s".to_string(),
            },
            json!({"kind": "gate_failed", "summary": "s"}),
        ),
        (
            BlockedReason::BudgetExceeded {
                detail: "d".to_string(),
            },
            json!({"kind": "budget_exceeded", "detail": "d"}),
        ),
        (
            BlockedReason::NeedsHuman {
                why: "w".to_string(),
            },
            json!({"kind": "needs_human", "why": "w"}),
        ),
        (
            BlockedReason::Infra {
                detail: "d".to_string(),
            },
            json!({"kind": "infra", "detail": "d"}),
        ),
        (
            BlockedReason::AuditRejected {
                detail: "score 86/95".to_string(),
            },
            json!({"kind": "audit_rejected", "detail": "score 86/95"}),
        ),
    ];
    for (reason, wire) in &cases {
        assert_eq!(wire["kind"], json!(reason.kind()), "{reason:?}");
    }
    assert_wire(&cases);
    assert!(rejects::<BlockedReason>(json!({"kind": "nope"})));
}

#[test]
fn unblock_tier() {
    let cases = [
        (UnblockTier::T1Classify, "T1Classify", "T1"),
        (UnblockTier::T2Mechanical, "T2Mechanical", "T2"),
        (UnblockTier::T3FreshContext, "T3FreshContext", "T3"),
        (UnblockTier::T4Decompose, "T4Decompose", "T4"),
        (UnblockTier::T5Escalate, "T5Escalate", "T5"),
    ];
    for (tier, wire, short) in cases {
        assert_wire(&[(tier, json!(wire))]);
        assert_eq!(tier.as_str(), short);
    }
}

#[test]
fn unblock_tier_ladder_climbs_one_rung_at_a_time_and_ends() {
    let mut chain = vec![UnblockTier::T1Classify];
    while let Some(next) = chain[chain.len() - 1].next() {
        assert!(next > chain[chain.len() - 1], "ladder must only climb");
        chain.push(next);
        assert!(chain.len() <= 5, "ladder does not terminate");
    }
    assert_eq!(
        chain,
        vec![
            UnblockTier::T1Classify,
            UnblockTier::T2Mechanical,
            UnblockTier::T3FreshContext,
            UnblockTier::T4Decompose,
            UnblockTier::T5Escalate,
        ]
    );
    assert_eq!(UnblockTier::T5Escalate.next(), None);
}

#[test]
fn unblock_action_is_tagged_by_action() {
    let plan = SubtaskPlan {
        title: "t".to_string(),
        description: "d".to_string(),
        acceptance: vec!["a".to_string()],
    };
    assert_wire(&[
        (
            UnblockAction::WaitForAccount { until: Some(5) },
            json!({"action": "wait_for_account", "until": 5}),
        ),
        (
            UnblockAction::SwitchAccount,
            json!({"action": "switch_account"}),
        ),
        (UnblockAction::RerunTests, json!({"action": "rerun_tests"})),
        (
            UnblockAction::RebaseAndRegate,
            json!({"action": "rebase_and_regate"}),
        ),
        (
            UnblockAction::WaitForDependency {
                task_id: "t".to_string(),
            },
            json!({"action": "wait_for_dependency", "task_id": "t"}),
        ),
        (
            UnblockAction::RetryFreshContext {
                digest: "g".to_string(),
            },
            json!({"action": "retry_fresh_context", "digest": "g"}),
        ),
        (
            UnblockAction::SpawnSubtasks { plans: vec![plan] },
            json!({"action": "spawn_subtasks", "plans": [
                {"title": "t", "description": "d", "acceptance": ["a"]}
            ]}),
        ),
        (
            UnblockAction::EscalateHuman {
                why: "w".to_string(),
            },
            json!({"action": "escalate_human", "why": "w"}),
        ),
        (
            UnblockAction::FreezeRoot {
                why: "w".to_string(),
            },
            json!({"action": "freeze_root", "why": "w"}),
        ),
        (
            UnblockAction::RetryInfra {
                detail: "d".to_string(),
            },
            json!({"action": "retry_infra", "detail": "d"}),
        ),
    ]);
}
