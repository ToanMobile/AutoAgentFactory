//! Pure path canonicalization, symlink-chain inspection, regex escaping, and
//! `PATH` binary lookup shared by executor sandboxes and host gate confinement.

use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

use aaf_domain::ports::{PortError, PortResult};

/// More links than this on one path is a loop.
const MAX_LINKS: usize = 40;

/// First regular file called `name` in the directories of `path_var`. Looking
/// the binary up ourselves skips any shell function or alias of that name.
pub fn find_on_path(path_var: &OsStr, name: &str) -> Option<PathBuf> {
    std::env::split_paths(path_var)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Canonical form of an absolute path that may not exist yet: every link is
/// followed, also one whose target does not exist (the agent could create
/// that target), the deepest existing ancestor is resolved and the remaining
/// components are appended.
pub fn canonical(raw: &str) -> PortResult<PathBuf> {
    let (followed, _) = follow(raw)?;
    existing_ancestor_resolved(&followed, raw)
}

/// `raw` resolved one link at a time, the way the kernel walks it, and the
/// link entries met on the way. Unlike `fs::canonicalize` this goes on past
/// a link whose target is missing. From the first entry that cannot be
/// looked at, the rest is taken as written.
fn follow(raw: &str) -> PortResult<(PathBuf, Vec<PathBuf>)> {
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err(PortError::Invalid(format!(
            "sandbox path is not absolute: {raw:?}"
        )));
    }
    let unresolvable = || PortError::Invalid(format!("sandbox path cannot be resolved: {raw:?}"));
    // Components still to walk, the next one last.
    let push_all = |todo: &mut Vec<OsString>, path: &Path| {
        let names = path.components().filter_map(|c| match c {
            Component::Normal(name) => Some(name.to_os_string()),
            Component::ParentDir => Some(OsString::from("..")),
            _ => None,
        });
        let names: Vec<OsString> = names.collect();
        todo.extend(names.into_iter().rev());
    };
    let mut todo = Vec::new();
    push_all(&mut todo, path);
    let mut done = PathBuf::from("/");
    let mut links = Vec::new();
    let mut missing = false;
    while let Some(name) = todo.pop() {
        if name == ".." {
            if missing {
                return Err(unresolvable());
            }
            done.pop();
            continue;
        }
        let next = done.join(&name);
        if !missing {
            match std::fs::symlink_metadata(&next) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    if links.len() >= MAX_LINKS {
                        return Err(unresolvable());
                    }
                    let target = std::fs::read_link(&next).map_err(|_| unresolvable())?;
                    links.push(next);
                    if target.is_absolute() {
                        done = PathBuf::from("/");
                    }
                    push_all(&mut todo, &target);
                    continue;
                }
                Ok(_) => {}
                // Nothing below an entry that is not there (or cannot be
                // looked at) can be a link today.
                Err(_) => missing = true,
            }
        }
        done = next;
    }
    Ok((done, links))
}

/// The link entries on the way to `raw` (see [`follow`]), each named by its
/// resolved directory and its own name.
pub(crate) fn links_to(raw: &str) -> PortResult<Vec<PathBuf>> {
    let (_, links) = follow(raw)?;
    links
        .iter()
        .map(|link| canonical_entry(path_str(link)?))
        .collect()
}

/// `path` with its deepest existing ancestor made canonical and the rest
/// appended. `raw` is what the caller asked about, for the error.
fn existing_ancestor_resolved(path: &Path, raw: &str) -> PortResult<PathBuf> {
    let mut missing: Vec<&OsStr> = Vec::new();
    let mut existing = path;
    loop {
        if let Ok(resolved) = std::fs::canonicalize(existing) {
            let mut full = resolved;
            full.extend(missing.iter().rev());
            return Ok(full);
        }
        // Only plain names may follow the resolved part: a `..` after a
        // component that does not exist cannot be resolved.
        match (existing.components().next_back(), existing.parent()) {
            (Some(Component::Normal(name)), Some(parent)) => {
                missing.push(name);
                existing = parent;
            }
            _ => {
                return Err(PortError::Invalid(format!(
                    "sandbox path cannot be resolved: {raw:?}"
                )));
            }
        }
    }
}

/// Canonical form of a directory entry: the directory is resolved, the last
/// component is not. A rule about an entry that is a symlink has to name the
/// link itself, or the link could be replaced.
pub(crate) fn canonical_entry(raw: &str) -> PortResult<PathBuf> {
    let path = Path::new(raw);
    match (path.parent().and_then(Path::to_str), path.file_name()) {
        (Some(parent), Some(name)) if path.is_absolute() => Ok(canonical(parent)?.join(name)),
        _ => Err(PortError::Invalid(format!(
            "sandbox path is not an absolute entry: {raw:?}"
        ))),
    }
}

/// Every directory entry from below `/` down to `path`, `path` included:
/// as written (a link on the way is an entry that could be replaced) and as
/// resolved (the directories that link leads through).
pub(crate) fn entries_to(path: &Path) -> PortResult<Vec<PathBuf>> {
    let mut entries = Vec::new();
    // A prefix that ends in `..` names no entry of its own; the resolved
    // form below covers where it leads.
    for prefix in path.ancestors().filter(|p| p.file_name().is_some()) {
        entries.push(canonical_entry(path_str(prefix)?)?);
    }
    let resolved = canonical(path_str(path)?)?;
    let below_root = resolved.ancestors().filter(|p| p.parent().is_some());
    entries.extend(below_root.map(Path::to_path_buf));
    Ok(entries)
}

fn path_str(path: &Path) -> PortResult<&str> {
    let text = path
        .to_str()
        .ok_or_else(|| PortError::Invalid(format!("sandbox path is not UTF-8: {path:?}")))?;
    if text.chars().any(|c| c == '"' || c.is_control()) {
        return Err(PortError::Invalid(format!(
            "sandbox path contains a quote or control character: {text:?}"
        )));
    }
    Ok(text)
}

/// Escapes every character that is special in a POSIX extended regex.
pub(crate) fn regex_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        if ".[]()*+?^$|\\{}".contains(c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}
