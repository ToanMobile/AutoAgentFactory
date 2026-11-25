//! Partial-tree cleanup (recovering `0555` dirs without following symlinks)
//! and stderr truncation shared across [`super::stage`], [`super::device`],
//! and [`super::seed`].

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use super::stage::CloneSkip;
use crate::git::GitError;

pub(super) const MAX_WALK_DEPTH: usize = 128;
const MAX_STDERR_CHARS: usize = 200;

/// Truncates command stderr to its first non-empty line, capped at 200 characters.
pub(super) fn truncate_stderr_first_line(stderr: &str) -> String {
    let first = stderr
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let mut out = String::new();
    let mut chars = first.chars();
    for ch in chars.by_ref().take(MAX_STDERR_CHARS) {
        out.push(ch);
    }
    if chars.next().is_some() {
        out.push_str("...");
    }
    out
}

pub(super) fn cleanup_or_skip(dst: &Path, orig_reason: String) -> CloneSkip {
    match remove_partial_dst(dst) {
        Ok(()) => CloneSkip::after_create(orig_reason),
        Err(err) => CloneSkip::cleanup_failed(err.to_string(), orig_reason),
    }
}

pub(super) fn cleanup_or_git_err(dst: &Path, orig_err: GitError) -> GitError {
    match remove_partial_dst(dst) {
        Ok(()) => orig_err,
        Err(err) => GitError::Invalid(format!("cleanup_failed: {err} (after {orig_err})")),
    }
}

fn make_dir_owner_writable_no_symlink(dir: &Path) -> bool {
    let Ok(meta) = fs::symlink_metadata(dir) else {
        return false;
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(mode | 0o700));
    }
    #[cfg(not(unix))]
    {
        let mut perms = meta.permissions();
        if perms.readonly() {
            perms.set_readonly(false);
            let _ = fs::set_permissions(dir, perms);
        }
    }
    true
}

/// Walks top-down from `wt_root` through each parent component of `rel` (up to
/// `dst.parent()`), restoring owner `rwx` (`0o700` on Unix; clearing readonly
/// elsewhere) on each real directory without following symlinks or leaving `wt_root`.
///
/// Returns `Some(dst)` (with no trailing separator) only when `rel` is a non-empty
/// path of normal components and `wt_root` plus every parent component of `rel` are
/// real non-symlink directories inside `wt_root`.
pub(super) fn make_parent_chain_writable(wt_root: &Path, rel: &Path) -> Option<PathBuf> {
    if rel.is_absolute() {
        return None;
    }
    let mut comps = Vec::new();
    for comp in rel.components() {
        match comp {
            Component::Normal(name) => comps.push(name),
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return None;
            }
        }
    }
    let (last, parents) = comps.split_last()?;
    let clean_wt: PathBuf = wt_root.components().collect();
    if clean_wt.as_os_str().is_empty() || !make_dir_owner_writable_no_symlink(&clean_wt) {
        return None;
    }
    let mut cur = clean_wt.clone();
    for name in parents {
        cur.push(name);
        if !cur.starts_with(&clean_wt) || !make_dir_owner_writable_no_symlink(&cur) {
            return None;
        }
    }
    cur.push(last);
    cur.starts_with(&clean_wt).then_some(cur)
}

/// Restores owner permissions recursively without following symlinks so
/// read-only (`0555`/`0500`) partial clones can still be removed cleanly.
pub(super) fn make_writable_no_symlinks(path: &Path, depth: usize, deadline: Option<Instant>) {
    if depth > MAX_WALK_DEPTH {
        return;
    }
    if deadline.is_some_and(|d| Instant::now() >= d) {
        return;
    }
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };
    if meta.file_type().is_symlink() {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        if meta.is_dir() {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o700));
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    make_writable_no_symlinks(&entry.path(), depth + 1, deadline);
                }
            }
        } else if meta.is_file() {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o600));
        }
    }
    #[cfg(not(unix))]
    {
        let mut perms = meta.permissions();
        if perms.readonly() {
            perms.set_readonly(false);
            let _ = fs::set_permissions(path, perms);
        }
        if meta.is_dir()
            && let Ok(entries) = fs::read_dir(path)
        {
            for entry in entries.flatten() {
                make_writable_no_symlinks(&entry.path(), depth + 1, deadline);
            }
        }
    }
}

/// Removes a partially materialized clone target, recovering read-only (`0555`)
/// subdirectories without following symlinks.
#[doc(hidden)]
pub fn remove_partial_dst(dst: &Path) -> io::Result<()> {
    remove_partial_dst_bounded(dst, None)
}

/// Deadline-bounded variant of [`remove_partial_dst`] for S2.8 operations (e.g. `.cache` removal).
pub(super) fn remove_partial_dst_bounded(dst: &Path, deadline: Option<Instant>) -> io::Result<()> {
    if deadline.is_some_and(|d| Instant::now() >= d) {
        return Err(io::Error::new(io::ErrorKind::TimedOut, "timeout"));
    }
    let meta = match fs::symlink_metadata(dst) {
        Ok(m) => m,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    if meta.is_dir() && !meta.file_type().is_symlink() {
        match fs::remove_dir_all(dst) {
            Ok(()) => Ok(()),
            Err(_) => {
                make_writable_no_symlinks(dst, 0, deadline);
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "timeout"));
                }
                fs::remove_dir_all(dst)
            }
        }
    } else {
        match fs::remove_file(dst) {
            Ok(()) => Ok(()),
            Err(_) if !meta.file_type().is_symlink() => {
                make_writable_no_symlinks(dst, 0, deadline);
                fs::remove_file(dst)
            }
            Err(err) => Err(err),
        }
    }
}
