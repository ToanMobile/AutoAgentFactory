//! Five-tier unblocker (docs/PLAN.md C.9).
//!
//! T1 `classifier` turns a failed step into a [`aaf_domain::blocked::BlockedReason`];
//! `ladder` picks the cheapest tier that has not been tried for that failure
//! and records the decision. Nothing here runs a process or touches git: the
//! factory executes the returned action.

pub mod classifier;
pub mod ladder;
pub mod proposer;

pub use classifier::{FailureSignal, RerunResult, classify, failure_digest, is_pure_test_failure};
pub use ladder::{Ladder, QuotaRoute, UnblockDecision};
pub use proposer::{
    AgentSubtaskProposer, PROPOSER_EXECUTOR, SubtaskProposer, parse_plans, validate_plans,
};
