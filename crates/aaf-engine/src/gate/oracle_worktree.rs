//! Detached oracle worktree + live-overlay + selective production revert
//! used by the opt-in red->green oracle. Backed by a fixed-path persistent
//! [`WarmSlot`] (with ephemeral fallback under contention), never mutating the
//! live worktree or following symlinks outside `scratch`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use aaf_domain::gate::ChangeKind;

use super::changes::{self, Change};
use super::oracle::is_oracle_test_path;
use super::proc::git_bytes;
use super::{GIT_TIMEOUT, GateError};
use crate::git::hydration::load_base_worktree_config;
use crate::git::warm_slot::{
    SlotKind, WarmSlot, base_snapshots_dir, promote_from_pure_base_slot,
    restore_clean_base_snapshot,
};

#[path = "oracle_worktree_lexer.rs"]
mod lexer;

pub(super) use lexer::inline_cfg_test_spans;
pub(crate) use lexer::split_inline_cfg_test;

/// Directory under the data dir where earlier builds put one-shot oracle
/// scratch worktrees (`<common_id>-<pid>-<seq>`); `aaf gc` still sweeps the
/// ones a killed gate left there. The oracle itself now runs in a warm slot
/// (`warm-slots/oracle/<common_id>/`), which `aaf gc` sweeps as well.
pub const ORACLE_SCRATCH_DIR: &str = "oracle-scratch";

/// RAII guard for a detached oracle worktree backed by a persistent warm slot.
pub(crate) struct ScratchWorktree {
    slot: WarmSlot,
}

impl ScratchWorktree {
    pub(crate) fn create(
        repo_root: &Path,
        data_dir: &Path,
        common_id: &str,
        head_sha: &str,
        base_sha: &str,
    ) -> Result<Self, GateError> {
        let slot = WarmSlot::acquire(
            repo_root,
            data_dir,
            common_id,
            SlotKind::Oracle,
            head_sha,
            "oracle",
        )
        .map_err(|err| GateError::Git(err.to_string()))?;
        let scratch = Self { slot };
        if let Ok(wt_cfg) = load_base_worktree_config(repo_root, base_sha)
            && !wt_cfg.clone_dirs.is_empty()
        {
            let dirs: Vec<&str> = wt_cfg.clone_dirs.iter().map(String::as_str).collect();
            scratch.seed_from_base_snapshot(data_dir, common_id, base_sha, &dirs);
        }
        Ok(scratch)
    }

    pub(crate) fn path(&self) -> &Path {
        self.slot.path()
    }

    /// Best-effort CoW restore of pre-warmed base artifact directories into
    /// this oracle scratch worktree from the shared base snapshots directory.
    pub(crate) fn seed_from_base_snapshot(
        &self,
        data_dir: &Path,
        common_id: &str,
        base_sha: &str,
        artifact_dirs: &[&str],
    ) -> usize {
        let snaps_dir = base_snapshots_dir(data_dir, common_id);
        restore_clean_base_snapshot(self.slot.path(), &snaps_dir, base_sha, artifact_dirs)
            .unwrap_or(0)
    }

    /// Promotes build artifact directories ONLY when the underlying slot is at
    /// pure `base_sha` with a clean working tree (never from a PR/reverted tree).
    #[allow(dead_code)]
    pub(crate) fn promote_clean_base_artifacts(
        &self,
        data_dir: &Path,
        common_id: &str,
        base_sha: &str,
        artifact_dirs: &[&str],
    ) -> Result<(), GateError> {
        promote_from_pure_base_slot(&self.slot, data_dir, common_id, base_sha, artifact_dirs)
            .map_err(|err| GateError::Git(err.to_string()))
    }
}

fn is_overlay_excluded(path: &str) -> bool {
    Path::new(path).components().any(|comp| {
        matches!(
            comp,
            Component::Normal(s) if s == ".git" || s == "target" || s == "node_modules"
        )
    })
}

/// Validates that `rel_path` has only normal components and resolves inside `scratch`.
fn checked_rel_components(rel_path: &str) -> Result<Vec<&std::ffi::OsStr>, GateError> {
    let mut parts = Vec::new();
    for comp in Path::new(rel_path).components() {
        match comp {
            Component::Normal(p) => parts.push(p),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(GateError::Io(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("oracle refused unsafe path outside scratch: {rel_path:?}"),
                )));
            }
        }
    }
    if parts.is_empty() {
        return Err(GateError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "oracle refused empty relative path",
        )));
    }
    Ok(parts)
}

/// Prepares `scratch/<rel_path>` for writing: unlinks any symlinked intermediate
/// directory or symlinked destination so writes never escape `scratch`, and
/// verifies the canonical parent stays under `scratch`.
pub(super) fn prepare_safe_write_target(
    scratch: &Path,
    rel_path: &str,
) -> Result<PathBuf, GateError> {
    let parts = checked_rel_components(rel_path)?;
    let (file_name, dirs) = parts
        .split_last()
        .ok_or_else(|| GateError::Io(io::Error::new(io::ErrorKind::InvalidInput, "empty path")))?;
    let mut cur = scratch.to_path_buf();
    for dir in dirs {
        cur.push(dir);
        if let Ok(meta) = fs::symlink_metadata(&cur) {
            if meta.file_type().is_symlink() || !meta.is_dir() {
                fs::remove_file(&cur)?;
                fs::create_dir(&cur)?;
            }
        } else {
            fs::create_dir(&cur)?;
        }
    }
    let canon_parent = fs::canonicalize(&cur)?;
    if !canon_parent.starts_with(scratch) {
        return Err(GateError::Io(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("oracle path escaped scratch worktree: {rel_path:?}"),
        )));
    }
    let dst = canon_parent.join(file_name);
    if fs::symlink_metadata(&dst).is_ok_and(|m| m.file_type().is_symlink()) {
        fs::remove_file(&dst)?;
    }
    Ok(dst)
}

/// Safely removes `scratch/<rel_path>` without traversing through symlinked
/// parent directories outside `scratch`.
pub(super) fn safe_remove(scratch: &Path, rel_path: &str) -> Result<(), GateError> {
    let parts = checked_rel_components(rel_path)?;
    let (file_name, dirs) = parts
        .split_last()
        .ok_or_else(|| GateError::Io(io::Error::new(io::ErrorKind::InvalidInput, "empty path")))?;
    let mut cur = scratch.to_path_buf();
    for dir in dirs {
        cur.push(dir);
        let Ok(meta) = fs::symlink_metadata(&cur) else {
            return Ok(());
        };
        if meta.file_type().is_symlink() || !meta.is_dir() {
            // Never descend through a symlinked directory outside scratch.
            return Ok(());
        }
    }
    let canon_parent = fs::canonicalize(&cur)?;
    if !canon_parent.starts_with(scratch) {
        return Ok(());
    }
    let dst = canon_parent.join(file_name);
    if let Ok(meta) = fs::symlink_metadata(&dst)
        && (meta.is_file() || meta.file_type().is_symlink())
    {
        let _ = fs::remove_file(&dst);
    }
    Ok(())
}

fn copy_regular_file(repo_root: &Path, scratch: &Path, rel_path: &str) -> Result<(), GateError> {
    let src = repo_root.join(rel_path);
    if !fs::symlink_metadata(&src).is_ok_and(|meta| meta.is_file()) {
        return Ok(());
    }
    let dst = prepare_safe_write_target(scratch, rel_path)?;
    fs::copy(&src, &dst)?;
    Ok(())
}

/// Overlays uncommitted tracked edits and untracked files from `repo_root`
/// onto `scratch` (skipping `.git`, `target`, `node_modules`).
pub(crate) fn overlay_live_changes(repo_root: &Path, scratch: &Path) -> Result<(), GateError> {
    let live = changes::collect(repo_root, "HEAD", GIT_TIMEOUT)?;
    for ch in &live.changes {
        if is_overlay_excluded(&ch.path) {
            continue;
        }
        match ch.kind {
            ChangeKind::Deleted => safe_remove(scratch, &ch.path)?,
            ChangeKind::Renamed => {
                if let Some(old) = &ch.old_path
                    && !is_overlay_excluded(old)
                {
                    safe_remove(scratch, old)?;
                }
                copy_regular_file(repo_root, scratch, &ch.path)?;
            }
            ChangeKind::Added | ChangeKind::Modified => {
                copy_regular_file(repo_root, scratch, &ch.path)?;
            }
        }
    }
    Ok(())
}

/// Whether `rel_path` is a gitlink (mode 160000, a submodule pointer) in
/// `base_sha`: `git show` cannot print one, since its commit lives in the
/// submodule's repository.
pub(super) fn is_gitlink_at(
    repo_root: &Path,
    base_sha: &str,
    rel_path: &str,
) -> Result<bool, GateError> {
    let raw = git_bytes(
        repo_root,
        &[
            "--literal-pathspecs",
            "ls-tree",
            "-z",
            base_sha,
            "--",
            rel_path,
        ],
        GIT_TIMEOUT,
    )?;
    Ok(raw.split(|b| *b == 0).any(|entry| {
        let entry = String::from_utf8_lossy(entry);
        entry
            .split_once('\t')
            .is_some_and(|(meta, path)| meta.starts_with("160000 ") && path == rel_path)
    }))
}

/// Reverts only `prod_changes` inside `scratch` back to `base_sha`, preserving
/// test files, fixture directories and inline Rust `#[cfg(test)]` blocks (the
/// caller leaves those out of `prod_changes`). Manifests such as `Cargo.toml`
/// are production files and are reverted whole, `[dev-dependencies]`
/// included: a test that needs a new dev-dependency then fails to build,
/// which the classifier reports as compile-red, never as proven. Gitlinks
/// (submodule pointers) are skipped: there is no file to restore.
pub(crate) fn revert_prod_files(
    repo_root: &Path,
    scratch: &Path,
    base_sha: &str,
    prod_changes: &[&Change],
    inline_rust_tests: &BTreeSet<String>,
) -> Result<(), GateError> {
    let mut actions: BTreeMap<&str, bool> = BTreeMap::new();
    for ch in prod_changes {
        match ch.kind {
            ChangeKind::Added => {
                actions.entry(ch.path.as_str()).or_insert(false);
            }
            ChangeKind::Modified | ChangeKind::Deleted => {
                actions.insert(ch.path.as_str(), true);
            }
            ChangeKind::Renamed => {
                if !is_oracle_test_path(&ch.path) {
                    actions.entry(ch.path.as_str()).or_insert(false);
                }
                if let Some(old) = &ch.old_path {
                    actions.insert(old.as_str(), true);
                }
            }
        }
    }
    for (rel_path, existed_at_base) in actions {
        let inline_tail = if inline_rust_tests.contains(rel_path) {
            let head_path = repo_root.join(rel_path);
            if fs::symlink_metadata(&head_path).is_ok_and(|m| m.is_file()) {
                fs::read_to_string(&head_path)
                    .ok()
                    .and_then(|s| split_inline_cfg_test(&s).1)
            } else {
                None
            }
        } else {
            None
        };
        if existed_at_base {
            let base_bytes = match git_bytes(
                repo_root,
                &["show", &format!("{base_sha}:{rel_path}")],
                GIT_TIMEOUT,
            ) {
                Ok(bytes) => bytes,
                Err(_) if is_gitlink_at(repo_root, base_sha, rel_path)? => continue,
                Err(err) => return Err(err),
            };
            let dst = prepare_safe_write_target(scratch, rel_path)?;
            if let Some(tail) = inline_tail {
                let base_str = String::from_utf8_lossy(&base_bytes);
                let (base_prod, _) = split_inline_cfg_test(&base_str);
                let combined = if !base_prod.is_empty() && !base_prod.ends_with('\n') {
                    format!("{base_prod}\n{tail}")
                } else {
                    format!("{base_prod}{tail}")
                };
                fs::write(&dst, combined.as_bytes())?;
            } else {
                fs::write(&dst, &base_bytes)?;
            }
        } else if let Some(tail) = inline_tail {
            let dst = prepare_safe_write_target(scratch, rel_path)?;
            fs::write(&dst, tail.as_bytes())?;
        } else {
            safe_remove(scratch, rel_path)?;
        }
    }
    Ok(())
}
