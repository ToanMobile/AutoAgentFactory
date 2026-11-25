//! Re-exports of supervised process capture from [`crate::confine_core::proc`],
//! plus [`git_bytes`] mapped to [`GateError`].

use std::path::Path;
use std::time::Duration;

use super::GateError;
pub(crate) use crate::confine_core::proc::capture;
#[cfg(test)]
pub(crate) use crate::confine_core::proc::{Supervise, run_supervised};

pub(crate) fn git_bytes(
    repo: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<Vec<u8>, GateError> {
    crate::confine_core::proc::git_bytes(repo, args, timeout).map_err(GateError::Git)
}
