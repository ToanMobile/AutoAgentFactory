//! What the gate does about files that are on disk without being in the
//! fingerprint ([`aaf_fp::hidden_files`]).
//!
//! Tracked files behind an index flag (skip-worktree, assume-unchanged)
//! always block: they change what a tracked test contains.
//!
//! Files ignored only by a rule from outside the tree (`info/exclude`, a
//! global excludes file, an untracked `.gitignore` that ignores itself)
//! depend on who runs the gate. In a person's own checkout they are that
//! person's environment (tool caches, editor files): the gate runs and the
//! receipt says they were there. In an agent's worktree nothing vouches for
//! them, so they are deleted before the commands run and again after. A
//! release gate never signs beside one: where it does not delete, it blocks.

use std::fs;
use std::path::{Component, Path};

use aaf_fp::{HiddenFiles, IgnoredFile};

use super::{GIT_TIMEOUT, GateError};

/// What to do about files ignored only by a rule the fingerprint does not
/// cover.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HiddenPolicy {
    /// Run, and name them in the receipt. For a person's own checkout.
    #[default]
    Warn,
    /// Delete them before and after the commands. For a work tree an agent
    /// wrote, and only for one: in a developer's checkout this deletes
    /// whatever `info/exclude` and the global excludes file hide.
    Remove,
}

/// When the work tree is looked at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum When {
    BeforeCommands,
    AfterCommands,
}

/// What one look at the work tree found.
#[derive(Debug, Default)]
pub(crate) struct Found {
    /// Reasons that block the gate.
    pub blocks: Vec<String>,
    /// Lines for the receipt that leave the status alone.
    pub notes: Vec<String>,
}

fn ignored_only(ignored: Vec<IgnoredFile>) -> HiddenFiles {
    HiddenFiles {
        flagged: Vec::new(),
        ignored,
    }
}

/// Deletes `entry` (relative to `root`, a directory when it ends with `/`).
/// A symlink is removed as a link, and nothing is touched unless the
/// directory holding the entry really lies inside `root`.
pub(crate) fn delete(root: &Path, entry: &str) -> Result<(), String> {
    let relative = Path::new(entry.trim_end_matches('/'));
    let plain = relative
        .components()
        .all(|part| matches!(part, Component::Normal(_)));
    if !plain || relative.as_os_str().is_empty() {
        return Err("not a path inside the work tree".to_string());
    }
    let path = root.join(relative);
    // Resolved, so a directory on the way that is a link to somewhere else
    // is found out. `root` is canonical already.
    let parent = path.parent().unwrap_or(root);
    let inside = fs::canonicalize(parent).is_ok_and(|parent| parent.starts_with(root));
    if !inside {
        return Err("its directory is not inside the work tree".to_string());
    }
    let removed = match fs::symlink_metadata(&path) {
        // `remove_dir_all` does not follow links below the directory.
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(&path),
        Ok(_) => fs::remove_file(&path),
        Err(err) => Err(err),
    };
    removed.map_err(|err| err.to_string())
}

fn removed_note(removed: &[IgnoredFile], when: When) -> String {
    let mut sources: Vec<&str> = removed.iter().map(|file| file.source.as_str()).collect();
    sources.sort_unstable();
    sources.dedup();
    let paths: Vec<&str> = removed.iter().map(|file| file.path.as_str()).collect();
    let shown = paths.iter().take(5).copied().collect::<Vec<_>>().join(", ");
    let more = match paths.len().saturating_sub(5) {
        0 => String::new(),
        more => format!(" (+{more} more)"),
    };
    let moment = match when {
        When::BeforeCommands => "before the commands ran",
        When::AfterCommands => "that the commands left behind",
    };
    format!(
        "work tree: removed {} ignored file(s) {moment}, hidden from the fingerprint \
         only by {}: {shown}{more}",
        paths.len(),
        sources.join(", ")
    )
}

fn remove(root: &Path, ignored: Vec<IgnoredFile>, when: When, found: &mut Found) {
    let mut removed: Vec<IgnoredFile> = Vec::new();
    for file in ignored {
        // Listed as well as the directory it was in, which is gone.
        let gone_with_its_directory = removed
            .iter()
            .any(|dir| dir.path.ends_with('/') && file.path.starts_with(&dir.path));
        if gone_with_its_directory {
            continue;
        }
        match delete(root, &file.path) {
            Ok(()) => removed.push(file),
            Err(why) => found.blocks.push(format!(
                "work tree: {} is ignored only by {}, which the fingerprint does not \
                 cover, and could not be removed: {why}",
                file.path, file.source
            )),
        }
    }
    if !removed.is_empty() {
        found.notes.push(removed_note(&removed, when));
    }
}

/// Looks at `root` (canonical) and applies `policy`. With `release`, what
/// the policy would only name blocks instead: a release receipt unlocks a
/// store submission of the tree on disk, so it is not signed beside a file
/// that is not in the fingerprint.
pub(crate) fn inspect(
    root: &Path,
    policy: HiddenPolicy,
    when: When,
    release: bool,
) -> Result<Found, GateError> {
    inspect_with(root, policy, when, release, |dir| {
        aaf_fp::hidden_files(dir, GIT_TIMEOUT)
    })
}

pub(crate) fn inspect_with<F>(
    root: &Path,
    policy: HiddenPolicy,
    when: When,
    release: bool,
    mut scan: F,
) -> Result<Found, GateError>
where
    F: FnMut(&Path) -> Result<HiddenFiles, aaf_fp::FpError>,
{
    let hidden = scan(root)?;
    let mut found = Found::default();
    match policy {
        HiddenPolicy::Warn if release => found.blocks.extend(
            ignored_only(hidden.ignored)
                .reasons()
                .into_iter()
                .map(|reason| format!("work tree: {reason}")),
        ),
        HiddenPolicy::Warn => found.notes.extend(
            ignored_only(hidden.ignored)
                .reasons()
                .into_iter()
                .map(|reason| format!("warning: work tree: {reason}")),
        ),
        HiddenPolicy::Remove if !hidden.ignored.is_empty() => {
            remove(root, hidden.ignored, when, &mut found);
            // Removing an ignore file can uncover what it hid: whatever is
            // still hidden now blocks instead of running unsigned.
            if found.blocks.is_empty() {
                let left = scan(root)?.ignored;
                found.blocks.extend(
                    ignored_only(left)
                        .reasons()
                        .into_iter()
                        .map(|reason| format!("work tree: still there after removal: {reason}")),
                );
            }
        }
        HiddenPolicy::Remove => {}
    }
    let flagged = HiddenFiles {
        flagged: hidden.flagged,
        ignored: Vec::new(),
    };
    found.blocks.extend(
        flagged
            .reasons()
            .into_iter()
            .map(|reason| format!("work tree: {reason}")),
    );
    Ok(found)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn remove_policy_skips_second_scan_when_no_ignored_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let mut calls = 0usize;
        let found = inspect_with(
            &root,
            HiddenPolicy::Remove,
            When::BeforeCommands,
            false,
            |_| {
                calls += 1;
                Ok(HiddenFiles::default())
            },
        )
        .unwrap();
        assert!(found.blocks.is_empty());
        assert_eq!(calls, 1, "must not re-scan when hidden.ignored is empty");
    }

    #[test]
    fn remove_policy_rescans_once_after_removing_ignored_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        fs::write(root.join("planted.py"), "x\n").unwrap();
        let mut calls = 0usize;
        let found = inspect_with(
            &root,
            HiddenPolicy::Remove,
            When::BeforeCommands,
            false,
            |_| {
                calls += 1;
                if calls == 1 {
                    Ok(HiddenFiles {
                        flagged: Vec::new(),
                        ignored: vec![IgnoredFile {
                            path: "planted.py".to_string(),
                            source: ".git/info/exclude".to_string(),
                        }],
                    })
                } else {
                    Ok(HiddenFiles::default())
                }
            },
        )
        .unwrap();
        assert!(found.blocks.is_empty());
        assert_eq!(calls, 2);
        assert!(!root.join("planted.py").exists());
    }
}
