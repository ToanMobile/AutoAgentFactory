//! Doctor dry-run pre-clone checks (S7) for worktree dependency cache seeding.

use std::path::Path;
use std::time::Instant;

use aaf_domain::worktree_config::WorktreeConfig;

use super::checks::{SeedCandidate, SeedKind, detect_seed_candidates_timed};
use super::clone::{RealSeedBackend, SEED_TIMEOUT, SeedBackend, precheck_target};
use crate::git::cmd;

/// Dry-run outcome for `aaf doctor` (S7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DryRunItem {
    pub rel_dir: &'static str,
    pub kind: Option<SeedKind>,
    pub would_seed: bool,
    pub reason: Option<String>,
}

/// Runs S1–S2 pre-clone checks for `aaf doctor` against `HEAD` without cloning or writing.
pub fn dry_run_seed(
    repo_root: &Path,
    cfg: &WorktreeConfig,
    worktrees_dir: &Path,
) -> Result<Vec<DryRunItem>, String> {
    dry_run_seed_with(repo_root, cfg, worktrees_dir, "HEAD", &RealSeedBackend)
}

/// Testable core of [`dry_run_seed`].
pub fn dry_run_seed_with(
    repo_root: &Path,
    cfg: &WorktreeConfig,
    worktrees_dir: &Path,
    base_ref: &str,
    backend: &dyn SeedBackend,
) -> Result<Vec<DryRunItem>, String> {
    let resolved_sha = cmd::git(
        repo_root,
        &["rev-parse", "--verify", &format!("{base_ref}^{{commit}}")],
    )
    .map_err(|e| format!("no commit at {base_ref}: {e}"))?;
    let deadline = Instant::now() + SEED_TIMEOUT;
    let candidates = detect_seed_candidates_timed(repo_root, &resolved_sha, deadline)?;
    let mut items = Vec::with_capacity(candidates.len());
    for cand in candidates {
        match cand {
            SeedCandidate::Skipped { rel_dir, reason } => items.push(DryRunItem {
                rel_dir,
                kind: None,
                would_seed: false,
                reason: Some(reason),
            }),
            SeedCandidate::Target(target) => {
                let res = precheck_target(
                    repo_root,
                    None,
                    worktrees_dir,
                    cfg,
                    &resolved_sha,
                    &target,
                    deadline,
                    backend,
                );
                match res {
                    Ok(_) => items.push(DryRunItem {
                        rel_dir: target.rel_dir,
                        kind: Some(target.kind),
                        would_seed: true,
                        reason: None,
                    }),
                    Err(reason) => items.push(DryRunItem {
                        rel_dir: target.rel_dir,
                        kind: Some(target.kind),
                        would_seed: false,
                        reason: Some(reason),
                    }),
                }
            }
        }
    }
    Ok(items)
}
