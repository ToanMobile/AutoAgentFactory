//! Dual-Frontier Adversarial Plan Debate state machine, schema validation,
//! and prompt framing (`docs/PLAN_DUAL_FRONTIER.md` §D.2).

pub mod prompt;
pub mod schema;
pub mod state;

/// Prefix of `BlockedReason::NeedsHuman::why` when Plan Debate stalls or
/// exhausts rounds and requires human steering or explicit approval.
pub const PLAN_DEBATE_STALLED_PREFIX: &str = "plan debate needs human decision:";

/// Key tag embedded inside `plan_approved:` context notes carrying the
/// canonical SHA-256 hash of the converged [`aaf_domain::pipeline::PlanContract`].
pub const PLAN_CONTRACT_HASH_TAG: &str = "plan_contract_hash=";

/// Maximum consecutive failed Plan Debate turns before stalling for human input.
pub const MAX_CONSECUTIVE_TURN_FAILS: u32 = 2;

pub use prompt::{
    build_author_prompt, build_challenger_prompt, build_plan_format_repair_prompt,
    build_plan_format_repair_prompt_with_prior, format_plan_for_implementer,
};
pub use schema::{
    canonical_plan_hash, extract_plan_contract_hash, format_plan_approved_note,
    normalize_repo_path, plan_contract_json_schema, plan_critique_json_schema,
    strip_for_downstream, validate_and_normalize_plan, validate_plan_critique,
};
pub use state::{
    CritiqueEvaluation, DebateNext, DebateRole, DebateSnapshot, evaluate_critique,
    is_human_unblock_event, next_step, replay_debate_state,
};
