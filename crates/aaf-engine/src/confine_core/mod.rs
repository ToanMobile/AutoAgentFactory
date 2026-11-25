//! Foundation confinement, supervised process capture, hidden-file inspection,
//! and base-ref/repo-id helpers shared across `gate`, `git`, `proof`,
//! `throttle`, and `executors` without circular dependencies.

pub mod git_refs;
pub mod hidden;
pub mod path;
pub(crate) mod proc;
mod shell;

#[cfg(test)]
mod tests;

use std::time::Duration;

pub use path::find_on_path;
pub use shell::{ConfineError, ConfineMode, probe};
pub(crate) use shell::{REASON_NO_DATA_DIR, shell_argv};

/// Budget for one git call of the gate or proof verification. Larger than
/// `aaf_fp::GIT_TIMEOUT` because fingerprinting or diffing a big tree is not a
/// ten-second job.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(60);
