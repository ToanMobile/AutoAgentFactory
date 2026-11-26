//! Why a task is blocked and what the unblocker ladder may do about it.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BlockedReason {
    Quota { reset_at: Option<i64> },
    Dependency { task_id: String },
    MergeConflict { detail: String },
    FlakyTest { detail: String },
    Stuck { detail: String },
    GateFailed { summary: String },
    BudgetExceeded { detail: String },
    NeedsHuman { why: String },
    Infra { detail: String },
    AuditRejected { detail: String },
}

impl BlockedReason {
    pub fn kind(&self) -> &'static str {
        match self {
            BlockedReason::Quota { .. } => "quota",
            BlockedReason::Dependency { .. } => "dependency",
            BlockedReason::MergeConflict { .. } => "merge_conflict",
            BlockedReason::FlakyTest { .. } => "flaky_test",
            BlockedReason::Stuck { .. } => "stuck",
            BlockedReason::GateFailed { .. } => "gate_failed",
            BlockedReason::BudgetExceeded { .. } => "budget_exceeded",
            BlockedReason::NeedsHuman { .. } => "needs_human",
            BlockedReason::Infra { .. } => "infra",
            BlockedReason::AuditRejected { .. } => "audit_rejected",
        }
    }
}

/// The five rungs of the unblocker ladder, cheapest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum UnblockTier {
    /// Deterministic classification of the failure.
    T1Classify,
    /// Mechanical fix: cooldown and switch account, rerun tests, rebase.
    T2Mechanical,
    /// Rerun the task in a fresh context with a failure digest.
    T3FreshContext,
    /// Split into depth-1 subtasks that share the root budget.
    T4Decompose,
    /// Hand over to a human.
    T5Escalate,
}

impl UnblockTier {
    pub fn next(self) -> Option<UnblockTier> {
        match self {
            UnblockTier::T1Classify => Some(UnblockTier::T2Mechanical),
            UnblockTier::T2Mechanical => Some(UnblockTier::T3FreshContext),
            UnblockTier::T3FreshContext => Some(UnblockTier::T4Decompose),
            UnblockTier::T4Decompose => Some(UnblockTier::T5Escalate),
            UnblockTier::T5Escalate => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            UnblockTier::T1Classify => "T1",
            UnblockTier::T2Mechanical => "T2",
            UnblockTier::T3FreshContext => "T3",
            UnblockTier::T4Decompose => "T4",
            UnblockTier::T5Escalate => "T5",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubtaskPlan {
    pub title: String,
    pub description: String,
    pub acceptance: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum UnblockAction {
    WaitForAccount { until: Option<i64> },
    SwitchAccount,
    RerunTests,
    RebaseAndRegate,
    WaitForDependency { task_id: String },
    RetryFreshContext { digest: String },
    SpawnSubtasks { plans: Vec<SubtaskPlan> },
    EscalateHuman { why: String },
    FreezeRoot { why: String },
    RetryInfra { detail: String },
}
