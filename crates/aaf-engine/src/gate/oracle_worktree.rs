//! Throwaway detached worktree + live-overlay + selective production revert
//! used by the opt-in red->green oracle. Never mutates the live worktree,
//! never follows symlinks outside `scratch`, and never runs global
//! `git worktree prune`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aaf_domain::gate::ChangeKind;

use super::changes::{self, Change};
use super::oracle::{is_cfg_test_attr_line, is_oracle_test_path};
use super::proc::git_bytes;
use super::{GIT_TIMEOUT, GateError};

static WORKTREE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Targeted RAII guard for a throwaway detached git worktree.
pub(crate) struct ScratchWorktree {
    repo_root: PathBuf,
    path: PathBuf,
}

impl ScratchWorktree {
    pub(crate) fn create(repo_root: &Path) -> Result<Self, GateError> {
        let tmp = fs::canonicalize(std::env::temp_dir()).unwrap_or_else(|_| std::env::temp_dir());
        let pid = std::process::id();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let mut path = loop {
            let seq = WORKTREE_SEQ.fetch_add(1, Ordering::Relaxed);
            let candidate = tmp.join(format!("aaf-oracle-wt-{pid}-{seq}-{nanos}"));
            if !candidate.exists() {
                break candidate;
            }
        };
        let path_str = path.to_string_lossy().into_owned();
        git_bytes(
            repo_root,
            &["worktree", "add", "--detach", &path_str, "HEAD"],
            GIT_TIMEOUT,
        )?;
        path = fs::canonicalize(&path)?;
        Ok(Self {
            repo_root: repo_root.to_path_buf(),
            path,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchWorktree {
    fn drop(&mut self) {
        let p = self.path.to_string_lossy().into_owned();
        let _ = git_bytes(
            &self.repo_root,
            &["worktree", "remove", "--force", &p],
            Duration::from_secs(30),
        );
        if self.path.exists() {
            let _ = fs::remove_dir_all(&self.path);
            let _ = git_bytes(
                &self.repo_root,
                &["worktree", "remove", &p],
                Duration::from_secs(30),
            );
        }
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
fn prepare_safe_write_target(scratch: &Path, rel_path: &str) -> Result<PathBuf, GateError> {
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
fn safe_remove(scratch: &Path, rel_path: &str) -> Result<(), GateError> {
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

/// Splits a Rust source file at the **last** top-level/indented `#[cfg(test)]`
/// attribute line outside `/* ... */` block comments so an early dummy
/// `#[cfg(test)]` cannot shield production code below it.
pub(crate) fn split_inline_cfg_test(source: &str) -> (&str, Option<&str>) {
    let mut offset = 0usize;
    let mut in_block_comment = false;
    let mut last_match = None;
    for segment in source.split_inclusive('\n') {
        let line = segment.trim_end_matches(['\r', '\n']);
        if !in_block_comment && is_cfg_test_attr_line(line) {
            last_match = Some(offset);
        }
        in_block_comment = update_block_comment(line, in_block_comment);
        offset += segment.len();
    }
    match last_match {
        Some(idx) => (&source[..idx], Some(&source[idx..])),
        None => (source, None),
    }
}

fn update_block_comment(line: &str, mut in_comment: bool) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if !in_comment && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            break;
        }
        if !in_comment && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            in_comment = true;
            i += 2;
            continue;
        }
        if in_comment && bytes[i] == b'*' && bytes[i + 1] == b'/' {
            in_comment = false;
            i += 2;
            continue;
        }
        i += 1;
    }
    in_comment
}

/// Whether `rel_path` is a gitlink (mode 160000, a submodule pointer) in
/// `base_sha`: `git show` cannot print one, since its commit lives in the
/// submodule's repository.
fn is_gitlink_at(repo_root: &Path, base_sha: &str, rel_path: &str) -> Result<bool, GateError> {
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
            fs::read_to_string(repo_root.join(rel_path))
                .ok()
                .and_then(|s| split_inline_cfg_test(&s).1.map(str::to_string))
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
                fs::write(&dst, format!("{base_prod}{tail}").as_bytes())?;
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
