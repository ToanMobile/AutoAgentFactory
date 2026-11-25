//! Seed reporting types and hydrate-time fail-closed entrypoint.

use std::path::Path;
use std::time::Instant;

use aaf_domain::worktree_config::WorktreeConfig;

use super::checks::{SeedKind, SeedTarget};
use super::clone::{SEED_TIMEOUT, SeedBackend, seed_worktree_with};
use crate::git::GitError;
use crate::git::hydration::cleanup::{make_parent_chain_writable, make_writable_no_symlinks};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedEntryReport {
    pub rel_dir: &'static str,
    pub kind: Option<SeedKind>,
    pub seeded: bool,
    pub files: usize,
    pub duration_ms: u128,
    pub skip_reason: Option<String>,
    /// Set whenever a post-creation failure (`created == true`) could not be cleaned up;
    /// escalates hydrate to `Err` (fail-closed) so no unverified tree remains.
    pub fatal_cleanup_error: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SeedReport {
    pub entries: Vec<SeedEntryReport>,
    pub duration_ms: u128,
}

impl SeedReport {
    pub fn summary_line(&self) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let parts: Vec<String> = self
            .entries
            .iter()
            .map(|e| {
                if e.seeded {
                    format!("{} ({} files, {}ms)", e.rel_dir, e.files, e.duration_ms)
                } else {
                    format!(
                        "{} skipped ({})",
                        e.rel_dir,
                        e.skip_reason.as_deref().unwrap_or("unknown")
                    )
                }
            })
            .collect();
        Some(format!(
            "aaf: seed {} [{}ms]",
            parts.join("; "),
            self.duration_ms
        ))
    }

    pub fn fatal_cleanup_error(&self) -> Option<&str> {
        self.entries
            .iter()
            .find_map(|e| e.fatal_cleanup_error.as_deref())
    }
}

/// Invoked by `hydrate_for_agent_inner`: runs seeding, prints summary line, and
/// fails closed (`Err`) if any post-creation cleanup failed (restoring owner
/// permissions on the parent chain up to `wt_root` and on `dst` first so
/// `worktree::remove` succeeds).
pub(crate) fn run_seed_during_hydrate(
    repo_root: &Path,
    wt_root: &Path,
    cfg: &WorktreeConfig,
    base_sha: &str,
    backend: &dyn SeedBackend,
) -> Result<(), GitError> {
    let report = seed_worktree_with(repo_root, wt_root, cfg, base_sha, SEED_TIMEOUT, backend);
    if let Some(line) = report.summary_line() {
        eprintln!("{line}");
    }
    if let Some(fatal) = report.fatal_cleanup_error() {
        for entry in &report.entries {
            if entry.fatal_cleanup_error.is_some()
                && let Some(dst) = make_parent_chain_writable(wt_root, Path::new(entry.rel_dir))
            {
                make_writable_no_symlinks(&dst, 0, None);
            }
        }
        return Err(GitError::Invalid(format!(
            "seed post-clone cleanup failed: {fatal}"
        )));
    }
    Ok(())
}

pub(super) fn skipped_entry(
    target: &SeedTarget,
    started: Instant,
    reason: String,
    fatal_cleanup_error: Option<String>,
) -> SeedEntryReport {
    SeedEntryReport {
        rel_dir: target.rel_dir,
        kind: Some(target.kind.clone()),
        seeded: false,
        files: 0,
        duration_ms: started.elapsed().as_millis(),
        skip_reason: Some(reason),
        fatal_cleanup_error,
    }
}
