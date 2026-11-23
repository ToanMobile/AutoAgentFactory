//! Pure domain types and port traits for AutoAgentFactory.
//!
//! This crate performs no I/O. Timestamps are Unix seconds (`i64`); every
//! caller gets the current time through [`ports::Clock`].

pub mod account;
pub mod blocked;
pub mod budget;
pub mod config;
pub mod dag;
pub mod event;
pub mod gate;
pub mod ports;
pub mod proof;
pub mod run;
pub mod sop;
pub mod task;
pub mod telegram;

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
