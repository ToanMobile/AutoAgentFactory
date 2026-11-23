//! Self-detect layer: turns crash traces and GitHub issues into task
//! candidates without creating duplicates or trusting outside text.
//!
//! - [`fingerprint`]: stable hash of a stack trace.
//! - [`dedupe`]: attach to an open task, suppress a recent closure, or create.
//! - [`repro`]: verifies a bug reproduces (fails every run) via manual operator
//!   pre-flight (`aaf repro` / `aaf board add --repro-cmd`). Never auto-executed
//!   by `aaf run` or `aaf gate`.
//! - [`github_issues`]: issues and comments from allowlisted authors only.

pub mod dedupe;
pub mod fingerprint;
pub mod github_issues;
pub mod repro;
