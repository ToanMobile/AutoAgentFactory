//! Automatic copy-on-write dependency cache seeding for fresh task worktrees.

pub mod checks;
pub mod clone;
mod git_bytes;
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
    SeedCandidate, SeedKind, SeedTarget, UNITY_EXCLUDED_ENTRIES, check_npm_lock_match,
    check_pods_lock_match, check_unity_editor_not_open, check_unity_state_match,
    detect_seed_candidates, overlaps_worktree_config, unity_editor_conflict,
};
pub use clone::{
    DryRunItem, RealSeedBackend, SEED_TIMEOUT, SeedBackend, SeedEntryReport, SeedReport,
    dry_run_seed, dry_run_seed_with, seed_worktree, seed_worktree_with,
};
pub use git_bytes::run_git_raw_bytes_timed;
