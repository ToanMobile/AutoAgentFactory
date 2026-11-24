//! The rules that keep a git directory's executable parts closed: what git
//! runs or obeys later, outside the sandbox, for the superproject and for
//! every submodule git dir below it.

use std::path::{Path, PathBuf};

use aaf_domain::ports::{PortError, PortResult};

use super::{path_str, regex_escape, sbpl_regex, sbpl_string};

/// Files in a git dir that git obeys (or, for `hooks`, runs). `commondir`
/// is read for any git dir and moves where git finds the others.
pub(super) const OBEYED: [&str; 4] = ["config", "config.worktree", "hooks", "commondir"];

/// Primary git-dir files never needed by normal commits (always write-denied).
const ALWAYS_PROTECTED_FILES: [&str; 4] = [
    "info/exclude",
    "info/exclude.lock",
    "packed-refs",
    "packed-refs.lock",
];

/// Primary git-dir state & protected branch refs write-denied when running in
/// a linked worktree (`worktrees/<name>/` and `refs/heads/aaf/*` stay writable).
const LINKED_PROTECTED_FILES: [&str; 11] = [
    "HEAD",
    "HEAD.lock",
    "index",
    "index.lock",
    "refs/heads/main",
    "refs/heads/main.lock",
    "refs/heads/master",
    "refs/heads/master.lock",
    "refs/heads/develop",
    "refs/heads/develop.lock",
    "refs/heads/release.lock",
];

/// Primary git-dir subpaths write-denied for linked worktrees (`release/*`).
const LINKED_PROTECTED_SUBPATHS: [&str; 1] = ["refs/heads/release"];

/// More directories than this below the `modules` dirs of one git dir is
/// refused rather than half walked: a submodule left out would lose its
/// rename guard.
const MAX_WALKED: usize = 10_000;

/// Directories `modules/` may nest in before a submodule git dir (names
/// with slashes) or between nested submodules.
const MAX_DEPTH: usize = 32;

/// Every existing submodule git dir below `git_dir`: a directory under
/// `modules/` (or `worktrees/<name>/modules/`) that holds `HEAD`, and the
/// same again below each of those for nested submodules. Links are not
/// followed.
pub(super) fn submodule_git_dirs(git_dir: &Path) -> PortResult<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut walked = 0;
    for dir in modules_dirs(git_dir)? {
        walk(&dir, 0, &mut found, &mut walked)?;
    }
    Ok(found)
}

/// `root/modules` and `root/worktrees/<name>/modules`.
fn modules_dirs(root: &Path) -> PortResult<Vec<PathBuf>> {
    let mut dirs = vec![root.join("modules")];
    for admin in sub_dirs(&root.join("worktrees"))? {
        dirs.push(admin.join("modules"));
    }
    Ok(dirs)
}

/// The directories (not links) directly in `dir`; none when it is missing.
fn sub_dirs(dir: &Path) -> PortResult<Vec<PathBuf>> {
    let unreadable =
        |err: std::io::Error| PortError::Invalid(format!("sandbox cannot list {dir:?}: {err}"));
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(unreadable(err)),
    };
    let mut dirs = Vec::new();
    for entry in entries {
        let entry = entry.map_err(unreadable)?;
        if entry.file_type().map_err(unreadable)?.is_dir() {
            dirs.push(entry.path());
        }
    }
    Ok(dirs)
}

fn walk(dir: &Path, depth: usize, found: &mut Vec<PathBuf>, walked: &mut usize) -> PortResult<()> {
    for sub in sub_dirs(dir)? {
        *walked += 1;
        if *walked > MAX_WALKED || depth > MAX_DEPTH {
            return Err(PortError::Invalid(format!(
                "sandbox: too many submodule directories below {dir:?}"
            )));
        }
        if std::fs::symlink_metadata(sub.join("HEAD")).is_ok() {
            for modules in modules_dirs(&sub)? {
                walk(&modules, depth + 1, found, walked)?;
            }
            found.push(sub);
        } else {
            walk(&sub, depth + 1, found, walked)?;
        }
    }
    Ok(())
}

/// Closed paths under `git_dir` whose ancestor directory entries must be
/// pinned against rename/deletion (without blocking sibling creation inside
/// `refs/heads`, so `refs/heads/aaf/*` stays writable).
pub(super) fn protected_closed_paths(git_dir: &Path, is_primary_checkout: bool) -> Vec<PathBuf> {
    let mut paths = vec![
        git_dir.join("refs/aaf"),
        git_dir.join("info/exclude"),
        git_dir.join("packed-refs"),
    ];
    if !is_primary_checkout {
        for rel in [
            "HEAD",
            "index",
            "refs/heads/main",
            "refs/heads/master",
            "refs/heads/develop",
            "refs/heads/release",
        ] {
            paths.push(git_dir.join(rel));
        }
    }
    paths
}

/// The deny rules for one git dir and its existing submodule git dirs
/// `subs` (see [`submodule_git_dirs`]).
pub(super) fn rules(
    git_dir: &Path,
    subs: &[PathBuf],
    is_primary_checkout: bool,
) -> PortResult<String> {
    let mut out = String::new();
    for root in std::iter::once(git_dir).chain(subs.iter().map(PathBuf::as_path)) {
        out.push_str(&obeyed_rules(root, subs)?);
        out.push_str(&worktree_rules(root)?);
    }
    // Host-owned snapshot refs under `refs/aaf/` stay read-only to agents.
    let refs_aaf = sbpl_string(&git_dir.join("refs/aaf"))?;
    out.push_str(&format!("(deny file-write* (subpath {refs_aaf}))\n"));
    out.push_str(&primary_state_rules(git_dir, is_primary_checkout)?);
    // The whole git dir moved into a writable place is no longer where the
    // rules above look.
    let own = sbpl_string(git_dir)?;
    out.push_str(&format!("(deny file-write* (literal {own}))\n"));
    Ok(out)
}

fn primary_state_rules(git_dir: &Path, is_primary_checkout: bool) -> PortResult<String> {
    let mut items = String::new();
    for rel in ALWAYS_PROTECTED_FILES {
        items.push_str(&format!(" (literal {})", sbpl_string(&git_dir.join(rel))?));
    }
    if !is_primary_checkout {
        for rel in LINKED_PROTECTED_FILES {
            items.push_str(&format!(" (literal {})", sbpl_string(&git_dir.join(rel))?));
        }
        for rel in LINKED_PROTECTED_SUBPATHS {
            items.push_str(&format!(" (subpath {})", sbpl_string(&git_dir.join(rel))?));
        }
    }
    Ok(format!("(deny file-write*{items})\n"))
}

/// The linked worktrees of `root` (the git dir or one existing submodule
/// git dir, audit a13 F7-R5-1). A linked worktree's admin dir carries its
/// own hooks and config, and finds its common dir (and so the hooks and
/// config there) through `commondir` and `gitdir`. The directory entries
/// themselves are closed too: a renamed admin dir (or a renamed
/// `worktrees`) is no longer where these rules look, and a new sibling entry
/// would be a worktree registration the agent made up. Git's own work goes
/// on below `worktrees/<name>/` and is not touched.
fn worktree_rules(root: &Path) -> PortResult<String> {
    let escaped = regex_escape(path_str(root)?);
    let mut out = String::new();
    for pattern in [
        "/[^/]+/(hooks|config|config\\.worktree)(/|$)",
        "/[^/]+/(commondir|gitdir)(/|$)",
        "(/[^/]+)?$",
    ] {
        let regex = sbpl_regex(&format!("^{escaped}/worktrees{pattern}"))?;
        out.push_str(&format!("(deny file-write* (regex {regex}))\n"));
    }
    Ok(out)
}

/// The obeyed files of `root` (the git dir or one submodule git dir), its
/// `modules` entries, and the obeyed files of every submodule git dir that
/// does not exist yet below them. A submodule's git dir carries its own
/// hooks and config, obeyed by the owner's next `git status` or
/// `git submodule update` (audit a13 F7-R3-1, F7-R4-1..3).
fn obeyed_rules(root: &Path, subs: &[PathBuf]) -> PortResult<String> {
    let hooks = sbpl_string(&root.join("hooks"))?;
    let mut files = String::new();
    for name in ["config", "config.worktree", "commondir"] {
        files.push_str(&format!(" (literal {})", sbpl_string(&root.join(name))?));
    }
    let escaped = regex_escape(path_str(root)?);
    // Renaming `modules` (or one submodule dir in it) away, editing it and
    // renaming it back would dodge every rule on what is below it.
    let modules = sbpl_regex(&format!("^{escaped}(/worktrees/[^/]+)?/modules$"))?;
    // Submodules that do not exist yet: anything that would be their hooks
    // or config. Existing ones have exact rules of their own (as `root`) and
    // are left out here, so a ref or a submodule called `config` in them
    // stays writable.
    let new = sbpl_regex(&format!(
        "^{escaped}(/worktrees/[^/]+)?/modules/.+/(hooks|config|config\\.worktree|commondir)(/|$)"
    ))?;
    let mut existing = String::new();
    for sub in subs
        .iter()
        .filter(|sub| sub.starts_with(root) && *sub != root)
    {
        existing.push_str(&format!(" (require-not (subpath {}))", sbpl_string(sub)?));
    }
    let new = if existing.is_empty() {
        format!("(regex {new})")
    } else {
        format!("(require-all (regex {new}){existing})")
    };
    Ok(format!(
        "(deny file-write* (subpath {hooks}){files} (regex {modules}))\n(deny file-write* {new})\n"
    ))
}
