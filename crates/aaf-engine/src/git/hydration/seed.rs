//! Automatic copy-on-write dependency cache seeding for fresh task worktrees.

pub mod checks;
pub mod clone;
mod dry_run;
mod git_bytes;
mod report;
mod verify;

use std::ffi::OsStr;

pub const SEED_CACHE_ENV: &str = "AAF_SEED_CACHE";

/// Returns false when `AAF_SEED_CACHE=0`; enabled by default otherwise.
pub fn seed_cache_enabled_from(val: Option<&OsStr>) -> bool {
    !matches!(val.and_then(OsStr::to_str).map(str::trim), Some("0"))
}

pub fn seed_cache_enabled() -> bool {
    seed_cache_enabled_from(std::env::var_os(SEED_CACHE_ENV).as_deref())
}

pub use checks::{
    NPM_EXCLUDED_ENTRIES, SeedCandidate, SeedKind, SeedTarget, UNITY_EXCLUDED_ENTRIES,
    check_npm_lock_match, check_pods_lock_match, check_unity_editor_not_open,
    check_unity_state_match, detect_seed_candidates, overlaps_worktree_config,
    unity_editor_conflict,
};
pub use clone::{RealSeedBackend, SEED_TIMEOUT, SeedBackend, seed_worktree, seed_worktree_with};
pub use dry_run::{DryRunItem, dry_run_seed, dry_run_seed_with};
pub use git_bytes::run_git_raw_bytes_timed;
pub(crate) use report::run_seed_during_hydrate;
pub use report::{SeedEntryReport, SeedReport};
