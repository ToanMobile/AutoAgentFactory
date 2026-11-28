//! Locked Pull Request merge gate and fail-closed post-merge cleanup.
//!
//! Sits above `git`, `gate`, `proof`, `config`, `db`, and `throttle` so the
//! lower-level `git` module remains cycle-free.

pub mod adopt_intake;
pub mod adopt_pin;
pub mod pr_cleanup;
mod pr_cleanup_db;
pub mod pr_merge;
#[cfg(test)]
mod pr_merge_ci_tests;
mod pr_merge_support;
// The adopt lane reads the base the way the merge gate does.
pub(crate) use pr_merge_support::{fetch_base, load_trusted_base_config};
pub(crate) mod pr_verify;

pub use pr_cleanup::{PrCleanupReport, cleanup_merged_pr_local, main_work_tree};
pub use pr_merge::{
    PrMergeOutcome, PrMergeRequest, merge_pr, merge_pr_with_check, merge_pr_with_cwd,
    sync_local_base,
};
