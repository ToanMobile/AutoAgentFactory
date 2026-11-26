//! Idempotent post-rebase copy of newly configured `[worktree].copy_files`.

use std::fs;
use std::path::Path;

use aaf_domain::worktree_config::is_secret_file;

use super::{load_base_worktree_config, resolve_entry};
use crate::git::GitError;

/// Copies any non-secret `[worktree].copy_files` entries defined at `base_sha`
/// that do not yet exist in `wt_root`. Reuses `resolve_entry` so path
/// validation, tracked `.gitignore`, and symlink-escape checks match fresh
/// hydration. Never overwrites existing files and never deletes the worktree.
pub fn hydrate_missing_copy_files(
    repo_root: &Path,
    wt_root: &Path,
    base_sha: &str,
) -> Result<(), GitError> {
    let cfg = load_base_worktree_config(repo_root, base_sha)?;
    for entry in &cfg.copy_files {
        if is_secret_file(entry) {
            return Err(GitError::Invalid(format!(
                "refusing to copy secret file {entry:?} into agent worktree; use [worktree].gate_copy_files"
            )));
        }
        let (src, dst) = resolve_entry(repo_root, wt_root, entry, false, "copy_files")?;
        if dst.symlink_metadata().is_ok() {
            continue;
        }
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| GitError::Invalid(format!("{}: {e}", parent.display())))?;
        }
        fs::copy(&src, &dst)
            .map_err(|e| GitError::Invalid(format!("copy {}: {e}", dst.display())))?;
    }
    Ok(())
}
