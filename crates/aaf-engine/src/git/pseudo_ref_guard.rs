//! SEC-125: a task agent can write its own linked-worktree admin dir, so a
//! pseudo-ref there (`ORIG_HEAD`, ...) may be a symref. A host `git rebase` /
//! `git reset` then updates the ref it points at (say `refs/heads/main`) to
//! the task's ungated commit. Every host step that writes pseudo-refs in a
//! task tree checks them first and refuses anything but a plain file.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use super::GitError;
use super::cmd::git;

/// Pseudo-refs a host rebase / reset / merge / cherry-pick may write.
const PSEUDO_REFS: [&str; 7] = [
    "ORIG_HEAD",
    "FETCH_HEAD",
    "AUTO_MERGE",
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "REBASE_HEAD",
];

/// Fails closed when a pseudo-ref in `tree`'s git dir is a symlink, a
/// non-regular file, a hard link, or a `ref:` symref. A missing one is fine.
pub(crate) fn ensure_plain_pseudo_refs(tree: &Path) -> Result<(), GitError> {
    let raw = PathBuf::from(git(tree, &["rev-parse", "--git-dir"])?);
    let git_dir = if raw.is_absolute() {
        raw
    } else {
        tree.join(raw)
    };
    for name in PSEUDO_REFS {
        check_one(&git_dir.join(name))?;
    }
    Ok(())
}

fn check_one(path: &Path) -> Result<(), GitError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    if !meta.file_type().is_file() || hard_linked(&meta) {
        return Err(refused(path, "is not a plain file"));
    }
    // A symref is `ref: <name>`; an oid line never starts that way.
    let mut head = Vec::new();
    fs::File::open(path)?.take(4096).read_to_end(&mut head)?;
    if head.trim_ascii_start().starts_with(b"ref:") {
        return Err(refused(path, "points at another ref"));
    }
    Ok(())
}

#[cfg(unix)]
fn hard_linked(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    meta.nlink() > 1
}

#[cfg(not(unix))]
fn hard_linked(_meta: &fs::Metadata) -> bool {
    false
}

fn refused(path: &Path, what: &str) -> GitError {
    GitError::Invalid(format!(
        "{} {what}: a task agent may have planted it so a host git step moves \
         another ref; check it, remove it, then `aaf board unblock` the task",
        path.display()
    ))
}
