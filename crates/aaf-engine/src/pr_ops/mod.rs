//! Locked Pull Request merge gate and fail-closed post-merge cleanup.
//!
//! Sits above `git`, `gate`, `proof`, `config`, `db`, and `throttle` so the
//! lower-level `git` module remains cycle-free.

pub mod pr_cleanup;
mod pr_cleanup_db;
pub mod pr_merge;
#[cfg(test)]
mod pr_merge_ci_tests;
mod pr_merge_support;
mod pr_temp_ref;

pub use pr_cleanup::{PrCleanupReport, cleanup_merged_pr_local, main_work_tree};
pub use pr_merge::{PrMergeOutcome, PrMergeRequest, merge_pr, merge_pr_with_cwd, sync_local_base};
