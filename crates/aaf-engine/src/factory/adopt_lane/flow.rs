//! How a lane turn ends: the three exits of every step.

use aaf_domain::ports::PortError;

use crate::gate::GateError;
use crate::git::GitError;

/// The adopted PR is merged at the pinned head.
pub(super) struct Done;

pub(super) enum Exit {
    /// The adoption cannot succeed as it stands: cancel with `code`.
    Abort { code: &'static str, reason: String },
    /// A temporary condition: try again later (counted, bounded).
    Retry { code: String, reason: String },
    /// Someone else already ended the task; touch nothing.
    Stop,
}

pub(super) type Flow<T> = Result<T, Exit>;

impl Exit {
    pub(super) fn abort(code: &'static str, reason: impl Into<String>) -> Exit {
        Exit::Abort {
            code,
            reason: reason.into(),
        }
    }

    pub(super) fn retry(code: &str, reason: impl Into<String>) -> Exit {
        Exit::Retry {
            code: code.to_string(),
            reason: reason.into(),
        }
    }
}

// Anything unexpected from a port is a reason to try again, never to merge.
impl From<PortError> for Exit {
    fn from(err: PortError) -> Exit {
        Exit::retry("store_error", err.to_string())
    }
}

impl From<GitError> for Exit {
    fn from(err: GitError) -> Exit {
        Exit::retry("git_error", err.to_string())
    }
}

impl From<GateError> for Exit {
    fn from(err: GateError) -> Exit {
        Exit::retry("gate_error", err.to_string())
    }
}
