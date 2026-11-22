//! Append-only factory events.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    TaskCreated,
    TaskStateChanged,
    RunStarted,
    RunFinished,
    GateResult,
    AccountCooldown,
    UnblockStep,
    BudgetFrozen,
    FactoryFrozen,
    FactoryUnfrozen,
    MergeResult,
    PrOpened,
    DetectorHit,
    CrossAudit,
    WebhookSent,
}

impl EventKind {
    pub const ALL: [EventKind; 15] = [
        EventKind::TaskCreated,
        EventKind::TaskStateChanged,
        EventKind::RunStarted,
        EventKind::RunFinished,
        EventKind::GateResult,
        EventKind::AccountCooldown,
        EventKind::UnblockStep,
        EventKind::BudgetFrozen,
        EventKind::FactoryFrozen,
        EventKind::FactoryUnfrozen,
        EventKind::MergeResult,
        EventKind::PrOpened,
        EventKind::DetectorHit,
        EventKind::CrossAudit,
        EventKind::WebhookSent,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::TaskCreated => "task_created",
            EventKind::TaskStateChanged => "task_state_changed",
            EventKind::RunStarted => "run_started",
            EventKind::RunFinished => "run_finished",
            EventKind::GateResult => "gate_result",
            EventKind::AccountCooldown => "account_cooldown",
            EventKind::UnblockStep => "unblock_step",
            EventKind::BudgetFrozen => "budget_frozen",
            EventKind::FactoryFrozen => "factory_frozen",
            EventKind::FactoryUnfrozen => "factory_unfrozen",
            EventKind::MergeResult => "merge_result",
            EventKind::PrOpened => "pr_opened",
            EventKind::DetectorHit => "detector_hit",
            EventKind::CrossAudit => "cross_audit",
            EventKind::WebhookSent => "webhook_sent",
        }
    }

    pub fn parse(s: &str) -> Option<EventKind> {
        EventKind::ALL.into_iter().find(|kind| kind.as_str() == s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactoryEvent {
    /// Assigned by the store on insert.
    pub id: Option<i64>,
    pub task_id: Option<String>,
    pub run_id: Option<String>,
    pub kind: EventKind,
    /// A JSON document; the store treats it as opaque text.
    pub payload_json: String,
    pub created_at: i64,
}

impl FactoryEvent {
    pub fn new(kind: EventKind, payload_json: impl Into<String>, now: i64) -> FactoryEvent {
        FactoryEvent {
            id: None,
            task_id: None,
            run_id: None,
            kind,
            payload_json: payload_json.into(),
            created_at: now,
        }
    }

    pub fn for_task(mut self, task_id: impl Into<String>) -> FactoryEvent {
        self.task_id = Some(task_id.into());
        self
    }

    pub fn for_run(mut self, run_id: impl Into<String>) -> FactoryEvent {
        self.run_id = Some(run_id.into());
        self
    }
}
