//! Shared job, gate-job, and outcome types passed between the scheduler and
//! worker threads.

use aaf_domain::gate::{GoldenScore, Receipt};
use aaf_domain::pipeline::{ReviewIssue, ReviewerConfig};
use aaf_fp::PartialAuditRecord;

use super::super::stage_router::ProviderKey;
use crate::git::TaskWorktree;
use crate::plan_debate::state::DebateRole;
use crate::unblocker::FailureSignal;

#[derive(Debug, Clone)]
pub(crate) struct PreGateAudit {
    pub tree_fp: String,
    pub base_sha: Option<String>,
    pub audit_provider: String,
    pub reviewer: String,
    pub same_account: bool,
    pub scores: Vec<GoldenScore>,
}

#[derive(Debug, Clone)]
pub(crate) struct GateJob {
    pub task_id: String,
    pub worktree: TaskWorktree,
    pub audited: bool,
    pub cached_receipt: Option<Box<Receipt>>,
}

pub(crate) enum Job {
    Agent {
        task_id: String,
    },
    PlanTurn {
        task_id: String,
        role: DebateRole,
        round: u32,
    },
    Audit {
        job: GateJob,
        lock_fp: String,
    },
    PostAudit {
        job: GateJob,
        receipt: Box<Receipt>,
        pre_audit: Option<Box<PreGateAudit>>,
    },
    DualReviewTurn {
        job: GateJob,
        receipt: Box<Receipt>,
        reviewer: ReviewerConfig,
        plan_hash: String,
    },
    AdjudicateTurn {
        job: GateJob,
        receipt: Box<Receipt>,
        pass_reviewer_id: String,
        findings: Vec<ReviewIssue>,
        partials: Vec<PartialAuditRecord>,
        plan_hash: String,
    },
    Unblock {
        job: GateJob,
        signal: Box<FailureSignal>,
        needs_account: bool,
    },
}

impl Job {
    /// A post-gate audit of `receipt` that audits afresh: no pre-gate
    /// verdict is reused (a retry, or a switch to another account).
    pub(crate) fn fresh_post_audit(job: GateJob, receipt: Receipt) -> Box<Job> {
        Box::new(Job::PostAudit {
            job,
            receipt: Box::new(receipt),
            pre_audit: None,
        })
    }

    pub(crate) fn task_id(&self) -> &str {
        match self {
            Job::Agent { task_id } | Job::PlanTurn { task_id, .. } => task_id,
            Job::Audit { job, .. }
            | Job::PostAudit { job, .. }
            | Job::DualReviewTurn { job, .. }
            | Job::AdjudicateTurn { job, .. }
            | Job::Unblock { job, .. } => &job.task_id,
        }
    }
}

/// Where the task is after a job.
pub(crate) enum Next {
    Gate(GateJob),
    GateAudited(GateJob, Box<PreGateAudit>),
    Ready,
    Switch,
    SwitchJob(Box<Job>),
    RetryAudit(Box<Job>),
    Wait {
        until: Option<i64>,
    },
    RetryAfter {
        until: i64,
        attempt: u32,
        detail: String,
    },
    Halt(String),
    Unblock {
        task_id: String,
        worktree: TaskWorktree,
        signal: Box<FailureSignal>,
    },
    DualReviewPartial {
        reviewer_id: String,
        inconclusive: Option<Box<PartialAuditRecord>>,
    },
    Blocked,
}

pub(crate) struct Outcome {
    pub task_id: String,
    pub provider: ProviderKey,
    pub next: Next,
    pub error: Option<String>,
}
