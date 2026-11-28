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
    ReworkRequested,
    ProofRecorded,
    PlanGenerated,
    PlanDebateTurn,
    PlanConverged,
    DualReviewTurn,
    ImplementerEscalated,
    FactoryHalted,
    PrFeedback,
    CostPaused,
    CostResumed,
    /// `aaf board adopt` took over a pull request opened outside the factory
    /// and pinned its head.
    PrAdopted,
    /// The adopt lane moved an adopted pull request to its next phase.
    AdoptStep,
    /// The adopt lane gave up on an adopted pull request (task `Cancelled`).
    AdoptAborted,
}

impl EventKind {
    pub const ALL: [EventKind; 29] = [
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
        EventKind::ReworkRequested,
        EventKind::ProofRecorded,
        EventKind::PlanGenerated,
        EventKind::PlanDebateTurn,
        EventKind::PlanConverged,
        EventKind::DualReviewTurn,
        EventKind::ImplementerEscalated,
        EventKind::FactoryHalted,
        EventKind::PrFeedback,
        EventKind::CostPaused,
        EventKind::CostResumed,
        EventKind::PrAdopted,
        EventKind::AdoptStep,
        EventKind::AdoptAborted,
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
            EventKind::ReworkRequested => "rework_requested",
            EventKind::ProofRecorded => "proof_recorded",
            EventKind::PlanGenerated => "plan_generated",
            EventKind::PlanDebateTurn => "plan_debate_turn",
            EventKind::PlanConverged => "plan_converged",
            EventKind::DualReviewTurn => "dual_review_turn",
            EventKind::ImplementerEscalated => "implementer_escalated",
            EventKind::FactoryHalted => "factory_halted",
            EventKind::PrFeedback => "pr_feedback",
            EventKind::CostPaused => "cost_paused",
            EventKind::CostResumed => "cost_resumed",
            EventKind::PrAdopted => "pr_adopted",
            EventKind::AdoptStep => "adopt_step",
            EventKind::AdoptAborted => "adopt_aborted",
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

/// What `aaf board adopt` records in the [`EventKind::PrAdopted`] event of an
/// adopted pull request. The adopt lane trusts `head_sha` and nothing else for
/// the head it reviews and merges.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrAdoptedPayload {
    pub pr_number: u64,
    pub url: String,
    /// The 40-hex head commit the adoption pinned (`refs/heads/aaf/<id>`).
    pub head_sha: String,
    pub head_ref: String,
    pub base_ref: String,
    pub author_login: String,
    /// What the chat said wrote the PR (`--author-executor`); logged only.
    #[serde(default)]
    pub declared_executor: Option<String>,
}

impl PrAdoptedPayload {
    /// True when `head_sha` is a 40-hex commit id.
    pub fn head_is_valid(&self) -> bool {
        self.head_sha.len() == 40 && self.head_sha.bytes().all(|b| b.is_ascii_hexdigit())
    }

    /// The pinned head in the form git prints it.
    pub fn pinned(&self) -> String {
        self.head_sha.to_ascii_lowercase()
    }
}
