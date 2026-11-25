//! Re-exports of hidden-file policy and inspection from
//! [`crate::confine_core::hidden`].

use std::path::Path;

use super::GateError;
#[cfg(test)]
pub(crate) use crate::confine_core::hidden::delete;
pub(crate) use crate::confine_core::hidden::{Found, When};
pub use crate::confine_core::hidden::{HiddenPolicy, is_benign_release_clutter};

pub(crate) fn inspect(
    root: &Path,
    policy: HiddenPolicy,
    when: When,
    release: bool,
) -> Result<Found, GateError> {
    crate::confine_core::hidden::inspect(root, policy, when, release).map_err(GateError::from)
}
