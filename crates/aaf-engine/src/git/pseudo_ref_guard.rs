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

const MAX_WALK: usize = 10_000;

/// Fails closed when a pseudo-ref in `tree`'s git dir, or (linked worktree)
/// any entry under its own `refs/`, is a symlink, a non-regular file, a hard
/// link, or a `ref:` symref. `--rebase-merges` writes `refs/rewritten/*`
/// there; a planted `ref: refs/heads/main` makes it delete `main`.
pub(crate) fn ensure_plain_pseudo_refs(tree: &Path) -> Result<(), GitError> {
    let git_dir = PathBuf::from(git(tree, &["rev-parse", "--absolute-git-dir"])?);
    for name in PSEUDO_REFS {
        check_one(&git_dir.join(name))?;
    }
    let refs = git_dir.join("refs");
    // Both paths come back resolved, so equal means the main checkout.
    if git_dir != common_dir(tree)? && fs::symlink_metadata(&refs).is_ok() {
        ensure_plain_ref_tree(&refs)?;
    }
    Ok(())
}

/// Walks `root` without following links (at most [`MAX_WALK`] entries);
/// every non-directory must pass [`check_one`].
fn ensure_plain_ref_tree(root: &Path) -> Result<(), GitError> {
    let mut stack = vec![root.to_path_buf()];
    let mut seen = 0usize;
    while let Some(path) = stack.pop() {
        seen += 1;
        if seen > MAX_WALK {
            return Err(refused(root, "holds more entries than the guard walks"));
        }
        if fs::symlink_metadata(&path)?.file_type().is_dir() {
            for entry in fs::read_dir(&path)? {
                stack.push(entry?.path());
            }
        } else {
            check_one(&path)?;
        }
    }
    Ok(())
}

fn common_dir(tree: &Path) -> Result<PathBuf, GitError> {
    let args = ["rev-parse", "--path-format=absolute", "--git-common-dir"];
    Ok(PathBuf::from(git(tree, &args)?))
}

/// Same check on the loose file of `full_ref` (`refs/heads/aaf/<task>`) in
/// the common dir, plus no symlinked directory on the way to it. `git
/// symbolic-ref HEAD` already reports the final target of a `ref:` chain,
/// but names a filesystem-symlinked ref by its first name.
pub(crate) fn ensure_plain_branch_ref(tree: &Path, full_ref: &str) -> Result<(), GitError> {
    let common = common_dir(tree)?;
    for dir in Path::new(full_ref).ancestors().skip(1) {
        let dir = common.join(dir);
        if fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(refused(&dir, "is a symlinked directory"));
        }
    }
    check_one(&common.join(full_ref))
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
