//! Guards host-side git ref updates against symlinked/hardlinked reflogs (SEC-116).

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::{fs, io};

use super::{GitError, cmd::git, worktree::resolved};

const MAX_REFLOG_WALKED: usize = 200_000;
const LINK_HINT: &str = "a task agent may have planted it to redirect git's reflog writes; \
     check it and remove it, then retry (for a factory task: `aaf board unblock` it)";

fn resolve_git_dirs(tree: &Path) -> Result<(PathBuf, PathBuf), GitError> {
    let raw = git(tree, &["rev-parse", "--git-common-dir", "--git-dir"])?;
    let mut lines = raw.lines().filter(|l| !l.is_empty());
    let (Some(c), Some(g), None) = (lines.next(), lines.next(), lines.next()) else {
        let msg = format!("unexpected git rev-parse output for {}", tree.display());
        return Err(GitError::Invalid(msg));
    };
    let abs = |s: &str| {
        let p = PathBuf::from(s);
        if p.is_absolute() { p } else { tree.join(p) }
    };
    Ok((abs(c), abs(g)))
}

/// Fails closed with [`GitError::Invalid`] if `<common>/logs` or `<git-dir>/logs`
/// contains a link, non-regular entry, unreadable entry, or exceeds the walk cap.
pub fn ensure_plain_reflogs(tree: &Path) -> Result<(), GitError> {
    ensure_plain_reflogs_all(&[tree])
}

/// Checks multiple trees while walking each shared `<common>/logs` at most once.
pub(crate) fn ensure_plain_reflogs_all(trees: &[&Path]) -> Result<(), GitError> {
    let mut checked: Vec<PathBuf> = Vec::new();
    for tree in trees {
        let (common, git_dir) = resolve_git_dirs(tree)?;
        for dir in [common, git_dir] {
            let key = resolved(&dir).join("logs");
            if checked.iter().any(|s| key == *s || key.starts_with(s)) {
                continue;
            }
            check_reflog_tree(&dir.join("logs"))?;
            checked.push(key);
        }
    }
    Ok(())
}

fn check_reflog_tree(root: &Path) -> Result<(), GitError> {
    let io_err = |p: &Path, e: io::Error| GitError::Invalid(format!("reflog {}: {e}", p.display()));
    let bad = |p: &Path, why: &str| {
        GitError::Invalid(format!("reflog {} {why}: {LINK_HINT}", p.display()))
    };
    let meta = match fs::symlink_metadata(root) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io_err(root, e)),
    };
    if meta.file_type().is_symlink() || !meta.file_type().is_dir() {
        return Err(bad(root, "root is not a plain directory"));
    }
    let mut walked = 0usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).map_err(|e| io_err(&dir, e))? {
            walked += 1;
            if walked > MAX_REFLOG_WALKED {
                let d = root.display();
                return Err(GitError::Invalid(format!(
                    "reflog walk exceeded {MAX_REFLOG_WALKED} entries under {d}"
                )));
            }
            let path = entry.map_err(|e| io_err(&dir, e))?.path();
            let meta = fs::symlink_metadata(&path).map_err(|e| io_err(&path, e))?;
            let ft = meta.file_type();
            if ft.is_symlink() {
                return Err(bad(&path, "is a link"));
            } else if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                #[cfg(unix)]
                if meta.nlink() > 1 {
                    let why = format!("is a link (nlink={})", meta.nlink());
                    return Err(bad(&path, &why));
                }
            } else {
                return Err(bad(&path, "is not a regular file"));
            }
        }
    }
    Ok(())
}
