//! Pure domain types and port traits for AutoAgentFactory.
//!
//! This crate performs no I/O. Timestamps are Unix seconds (`i64`); every
//! caller gets the current time through [`ports::Clock`].

pub mod account;
pub mod accounts_config;
pub mod blocked;
pub mod budget;
pub mod builtin_defaults;
pub mod commands_config;
pub mod config;
pub mod dag;
pub mod engine_contracts;
pub mod event;
pub mod gate;
pub mod head_lock_globs;
pub mod heavy;
pub mod keychain;
pub mod land_mode;
pub mod oracle_config;
pub mod pipeline;
pub mod ports;
pub mod prompt_fence;
pub mod proof;
pub mod proof_flow_config;
pub mod run;
pub mod sop;
pub mod task;
pub mod telegram;
pub mod worktree_config;

/// Errors raised by pure domain rules.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DomainError {
    #[error("subtask depth {depth} exceeds the maximum of {max}")]
    SubtaskDepthExceeded { depth: u8, max: u8 },
    #[error("invalid task transition {from} -> {to}")]
    InvalidTransition {
        from: &'static str,
        to: &'static str,
    },
    #[error("dependency {task} -> {blocked_by} would create a cycle")]
    DependencyCycle { task: String, blocked_by: String },
}
