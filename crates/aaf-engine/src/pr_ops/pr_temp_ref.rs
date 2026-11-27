//! RAII guard for unique temporary PR-fetch refs under `refs/aaf-merge/`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::git::cmd::git_raw;

static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

/// Unique temporary ref (`refs/aaf-merge/pr-{num}-{pid}-{seq}`); always
/// deleted on `Drop` so error paths never leak refs.
pub(super) struct TempRefGuard {
    repo_root: PathBuf,
    ref_name: String,
}

impl TempRefGuard {
    pub(super) fn new(repo_root: &Path, pr_number: u64) -> Self {
        let seq = SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed);
        let ref_name = format!("refs/aaf-merge/pr-{pr_number}-{}-{seq}", std::process::id());
        Self {
            repo_root: repo_root.to_path_buf(),
            ref_name,
        }
    }

    pub(super) fn as_str(&self) -> &str {
        &self.ref_name
    }
}

impl Drop for TempRefGuard {
    fn drop(&mut self) {
        let _ = git_raw(
            &self.repo_root,
            &["update-ref", "-d", "--no-deref", &self.ref_name],
        );
    }
}
