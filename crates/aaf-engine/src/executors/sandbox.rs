//! macOS Seatbelt confinement for agent runs.
//!
//! The profile allows everything by default, then denies every file write
//! outside a list of writable subtrees and every access to a list of denied
//! subtrees. SBPL applies the last matching rule, so the order of the
//! sections below is the policy.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use aaf_domain::ports::{PortError, PortResult, SandboxPolicy};

use super::pty::CommandSpec;

mod git_dir;

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Device nodes ordinary programs write to; without them even `> /dev/null`
/// fails under `(deny file-write*)`.
const WRITABLE_DEVICES: &str = r##"(allow file-write*
  (literal "/dev/null")
  (literal "/dev/zero")
  (literal "/dev/tty")
  (literal "/dev/ptmx")
  (literal "/dev/dtracehelper")
  (regex #"^/dev/ttys[0-9]+$")
  (regex #"^/dev/fd/[0-9]+$"))
"##;

/// Paths the executor adds to the caller's [`SandboxPolicy`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SandboxPaths {
    pub writable: Vec<String>,
    /// Narrow SBPL regexes allowed for writing (emitted before the deny section).
    pub writable_regex: Vec<String>,
    /// Closed for writing even inside a writable subtree: the entry itself
    /// (it cannot be replaced, renamed or deleted) and everything below it.
    pub write_denied: Vec<String>,
    /// Like `write_denied`, for every path that starts with the given one
    /// (`…/.cargo/config` closes `config` and `config.toml`).
    pub write_denied_prefix: Vec<String>,
    pub read_denied: Vec<String>,
    /// Directories inside a read-denied one that the run needs all the
    /// same. Readable, and not writable: neither they nor what they hold
    /// can be replaced, renamed or deleted.
    pub read_reopened: Vec<String>,
    /// Directories inside a read-denied one where the run may create and
    /// write files. The directory entry itself stays closed.
    pub write_reopened: Vec<String>,
    /// The run's own worktree: writable, and the only writable place under a
    /// task-worktrees directory.
    pub worktree: Option<String>,
    /// Git directories the run commits into: writable except for the parts
    /// git would later execute outside the sandbox (hooks and config).
    pub git_dirs: Vec<String>,
    /// The directory that holds every task worktree
    /// (`<root>/<repo_id>/<task_id>`). Nothing below it is writable except
    /// `worktree`.
    pub worktrees_root: Option<String>,
    /// Directories commands are looked up in outside the sandbox: the `PATH`
    /// of the process that starts the agent. One that lies inside a writable
    /// subtree is closed, because a file called `git` there is what aaf and
    /// the owner's shell would run next. Relative and empty entries are
    /// skipped: what they name depends on who looks.
    pub path_dirs: Vec<String>,
    /// The run's mark file ([`super::reaper::RunMarks`]): the profile lets
    /// the run write its data and nothing else about it, so the kernel's
    /// answer for that file tells this run's sandbox from every other one.
    pub run_mark: Option<String>,
    /// Optional macOS Keychain dir (e.g. `~/Library/Keychains`) to deny along
    /// with `com.apple.SecurityServer` and `com.apple.securityd.xpc` IPC.
    pub deny_keychain_dir: Option<String>,
}

/// Builds the SBPL profile for `policy` plus `extra`.
///
/// Every path is made canonical first: Seatbelt matches the resolved path, and
/// on macOS `/tmp` and `/var` are symlinks into `/private`. A path that is
/// relative or cannot be written safely inside an SBPL string is rejected
/// rather than dropped, because dropping a denied path would silently widen
/// the sandbox.
pub fn seatbelt_profile(policy: &SandboxPolicy, extra: &SandboxPaths) -> PortResult<String> {
    let policy_writable: &[String] = if policy.read_only {
        &[]
    } else {
        &policy.writable_paths
    };
    let mut writable = canonical_paths(policy_writable.iter().chain(&extra.writable))?;
    let git_dirs = canonical_paths(extra.git_dirs.iter())?;
    let denied = canonical_paths(policy.read_denied_paths.iter().chain(&extra.read_denied))?;
    let worktree = extra.worktree.as_deref().map(canonical).transpose()?;
    let worktrees_root = extra.worktrees_root.as_deref().map(canonical).transpose()?;
    let deny_keychain = extra
        .deny_keychain_dir
        .as_deref()
        .map(canonical)
        .transpose()?;
    if let Some(root) = &worktrees_root {
        // Another task's worktree is never writable, whoever listed it.
        writable.retain(|path| !path.starts_with(root));
    }

    let mut profile = String::from("(version 1)\n(allow default)\n(deny file-write*)\n");
    profile.push_str(WRITABLE_DEVICES);
    for path in writable.iter().chain(&git_dirs) {
        let path = sbpl_string(path)?;
        profile.push_str(&format!("(allow file-write* (subpath {path}))\n"));
    }
    for pattern in &extra.writable_regex {
        if pattern.is_empty() {
            return Err(PortError::Invalid(
                "sandbox writable_regex must not be empty".to_string(),
            ));
        }
        let regex = sbpl_regex(pattern)?;
        profile.push_str(&format!("(allow file-write* (regex {regex}))\n"));
    }
    // A writable ancestor (a temp dir, the main checkout) must not open
    // sibling task worktrees: close the whole directory, and reopen only this
    // run's own worktree when it is one of them. A worktree that is not below
    // the root is opened first, so that it cannot re-open the root.
    let own_is_task_worktree = match (&worktree, &worktrees_root) {
        (Some(own), Some(root)) => own != root && own.starts_with(root),
        _ => false,
    };
    let open_own = match &worktree {
        Some(own) => format!("(allow file-write* (subpath {}))\n", sbpl_string(own)?),
        None => String::new(),
    };
    if !own_is_task_worktree {
        profile.push_str(&open_own);
    }
    if let Some(root) = &worktrees_root {
        let root = sbpl_string(root)?;
        profile.push_str(&format!("(deny file-write* (subpath {root}))\n"));
    }
    if own_is_task_worktree {
        profile.push_str(&open_own);
    }
    // No allow rule may follow this point: everything below is a deny that
    // must hold inside every writable subtree above. In a normal checkout the
    // git dir is inside the worktree, so these come after the worktree allow.
    // Hooks and config are run or obeyed by every later git command,
    // including the user's own outside the sandbox; the same files exist per
    // linked worktree under `worktrees/<name>/`.
    let is_primary_checkout = |git_dir: &Path| {
        worktree
            .as_ref()
            .is_some_and(|wt| wt.join(".git") == git_dir)
    };
    let mut submodules = Vec::new();
    for git_dir in &git_dirs {
        let subs = git_dir::submodule_git_dirs(git_dir)?;
        profile.push_str(&git_dir::rules(
            git_dir,
            &subs,
            is_primary_checkout(git_dir),
        )?);
        submodules.extend(subs);
    }
    if let Some(worktree) = &worktree {
        // The entry itself, not what is below it: replacing the `.git` file
        // of a linked worktree, or moving the `.git` directory of a normal
        // checkout aside, would point later git commands at a git dir whose
        // hooks the agent wrote.
        let dot_git = sbpl_string(&worktree.join(".git"))?;
        profile.push_str(&format!("(deny file-write* (literal {dot_git}))\n"));
    }
    let mut symlink_targets = BTreeSet::new();
    // Every link met on the way to a closed path, including links between
    // links: replacing one would point the closed path somewhere else.
    let mut chain_links = BTreeSet::new();
    for raw in &extra.write_denied {
        let entry = canonical_entry(raw)?;
        let resolved = canonical(raw)?;
        let path = sbpl_string(&entry)?;
        profile.push_str(&format!("(deny file-write* (subpath {path}))\n"));
        chain_links.extend(links_to(raw)?);
        if resolved != entry {
            symlink_targets.insert(resolved);
        }
    }
    for raw in &extra.write_denied_prefix {
        let entry = canonical_entry(raw)?;
        let starts = sbpl_regex(&format!("^{}", regex_escape(path_str(&entry)?)))?;
        profile.push_str(&format!("(deny file-write* (regex {starts}))\n"));
        chain_links.extend(links_to(raw)?);
        let resolved = canonical(raw)?;
        if resolved != entry {
            symlink_targets.insert(resolved);
        }
        // Prefix entries can match existing symlinks (e.g. `.cargo/config.toml`
        // symlinked inside writable `.cargo`); close their resolved targets too.
        if let (Some(parent), Some(prefix)) = (
            entry.parent(),
            entry.file_name().and_then(std::ffi::OsStr::to_str),
        ) && let Ok(entries) = std::fs::read_dir(parent)
        {
            for dir_entry in entries.flatten() {
                if dir_entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(prefix))
                {
                    let candidate = dir_entry.path();
                    if let Some(cand_str) = candidate.to_str()
                        && let Ok(target) = canonical(cand_str)
                        && target != candidate
                    {
                        chain_links.extend(links_to(cand_str)?);
                        symlink_targets.insert(target);
                    }
                }
            }
        }
    }
    for target in &symlink_targets {
        let target = sbpl_string(target)?;
        profile.push_str(&format!("(deny file-write* (subpath {target}))\n"));
    }
    let mut roots: Vec<PathBuf> = writable.iter().chain(&git_dirs).cloned().collect();
    roots.extend(worktree.clone());
    for raw in &extra.write_reopened {
        roots.push(canonical(raw)?);
    }
    let opened = |path: &Path| roots.iter().any(|root| path.starts_with(root));
    // Everything a rule closes, as written by the caller. Each is only as
    // closed as the directories that lead to it (see `entries_to`).
    let mut closed: Vec<PathBuf> = denied.clone();
    closed.extend(worktrees_root.clone());
    // The obeyed files of every git dir, submodules included: renaming a
    // directory on the way to one would carry it out of its rules (F7-R4-2).
    for root in git_dirs.iter().chain(&submodules) {
        closed.extend(git_dir::OBEYED.iter().map(|name| root.join(name)));
    }
    for git_dir in &git_dirs {
        closed.extend(git_dir::protected_closed_paths(
            git_dir,
            is_primary_checkout(git_dir),
        ));
    }
    closed.extend(deny_keychain.clone());
    closed.extend(worktree.iter().map(|worktree| worktree.join(".git")));
    let listed = extra.write_denied.iter().chain(&extra.write_denied_prefix);
    closed.extend(listed.map(PathBuf::from));
    closed.extend(symlink_targets);
    let mut on_path = BTreeSet::new();
    for raw in &extra.path_dirs {
        let dir = Path::new(raw);
        if !dir.is_absolute() {
            continue;
        }
        // Fails closed: an entry that cannot be resolved now could become
        // one the agent decides the meaning of.
        let on_the_way = entries_to(dir)
            .map_err(|err| PortError::Invalid(format!("PATH entry {raw:?}: {err}")))?;
        if !on_the_way.iter().any(|entry| opened(entry)) {
            continue;
        }
        // The entry as PATH names it and, when that is a link, the
        // directory it leads to.
        on_path.insert(canonical_entry(raw)?);
        on_path.insert(canonical(raw)?);
        chain_links.extend(links_to(raw)?);
        closed.push(dir.to_path_buf());
    }
    for dir in on_path.iter().filter(|dir| opened(dir)) {
        let dir = sbpl_string(dir)?;
        profile.push_str(&format!("(deny file-write* (subpath {dir}))\n"));
    }
    // The link entries themselves, so none of them can be replaced. Links
    // outside every writable place (`/var`, `/tmp`) are closed already.
    for link in chain_links.iter().filter(|link| opened(link)) {
        let link = sbpl_string(link)?;
        profile.push_str(&format!("(deny file-write* (literal {link}))\n"));
    }
    closed.extend(chain_links);
    // A deny on what is inside a writable directory holds only while no
    // directory on the way to it can be renamed into another writable place,
    // changed there and renamed back: `(subpath root)` opens the entry of
    // `root` too. So the entry of every writable directory that leads to
    // something closed is closed itself. That forbids renaming, deleting and
    // replacing that one entry (and changing its mode, times and extended
    // attributes), not creating, renaming or deleting what is in it. One
    // that does not exist cannot be created; the executor makes the roots
    // builds need before the run.
    let mut guarded = BTreeSet::new();
    for path in &closed {
        guarded.extend(entries_to(path)?.into_iter().filter(|entry| opened(entry)));
    }
    for entry in &guarded {
        let entry = sbpl_string(entry)?;
        profile.push_str(&format!("(deny file-write* (literal {entry}))\n"));
    }
    // Last, so a denied directory stays closed even inside a writable one.
    for path in &denied {
        let path = sbpl_string(path)?;
        profile.push_str(&format!("(deny file-read* file-write* (subpath {path}))\n"));
    }
    if let Some(kc) = &deny_keychain {
        let kc = sbpl_string(kc)?;
        profile.push_str(&format!(
            "(deny file-read* file-write* (subpath {kc}))\n\
             (deny mach-lookup\n  \
               (global-name \"com.apple.SecurityServer\")\n  \
               (global-name \"com.apple.securityd.xpc\"))\n"
        ));
    }
    // The one exception to "no allow after the denies": what is reopened
    // inside a denied directory has to come after the rule that closes it.
    // The caller answers for these holding nothing that must stay closed.
    for raw in &extra.read_reopened {
        let path = sbpl_string(&canonical(raw)?)?;
        profile.push_str(&format!("(allow file-read* (subpath {path}))\n"));
    }
    for raw in &extra.write_reopened {
        let path = sbpl_string(&canonical(raw)?)?;
        profile.push_str(&format!(
            "(allow file-read* file-write* (subpath {path}))\n(deny file-write* (literal {path}))\n"
        ));
    }
    // Last, so no deny above can hide it: the kernel's answer for this one
    // file tells the run's sandbox from every other ([`super::reaper`]).
    // Data only: the agent can neither remove nor replace the mark to hide
    // what it leaves behind.
    if let Some(mark) = &extra.run_mark {
        let mark = canonical(mark)?;
        let mark = sbpl_string(&mark)?;
        profile.push_str(&format!("(allow file-write-data (literal {mark}))\n"));
    }
    Ok(profile)
}

/// Rewrites `command` to run under `/usr/bin/sandbox-exec` with `profile`.
pub fn wrap(command: &CommandSpec, profile: &str) -> PortResult<CommandSpec> {
    if !cfg!(target_os = "macos") {
        return Err(PortError::Unsupported(
            "the Seatbelt sandbox exists on macOS only".to_string(),
        ));
    }
    let mut args = vec!["-p".to_string(), profile.to_string(), "--".to_string()];
    args.push(command.program.clone());
    args.extend(command.args.iter().cloned());
    Ok(CommandSpec {
        program: SANDBOX_EXEC.to_string(),
        args,
        cwd: command.cwd.clone(),
        env: command.env.clone(),
        env_clear: command.env_clear,
    })
}

fn canonical_paths<'a>(paths: impl Iterator<Item = &'a String>) -> PortResult<Vec<PathBuf>> {
    let mut all: Vec<PathBuf> = Vec::new();
    for raw in paths {
        let path = canonical(raw)?;
        if !all.contains(&path) {
            all.push(path);
        }
    }
    Ok(all)
}

/// Canonical form of an absolute path that may not exist yet: every link is
/// followed, also one whose target does not exist (the agent could create
/// that target), the deepest existing ancestor is resolved and the remaining
/// components are appended.
pub fn canonical(raw: &str) -> PortResult<PathBuf> {
    let (followed, _) = follow(raw)?;
    existing_ancestor_resolved(&followed, raw)
}

/// More links than this on one path is a loop.
const MAX_LINKS: usize = 40;

/// `raw` resolved one link at a time, the way the kernel walks it, and the
/// link entries met on the way. Unlike `fs::canonicalize` this goes on past
/// a link whose target is missing. From the first entry that cannot be
/// looked at, the rest is taken as written.
fn follow(raw: &str) -> PortResult<(PathBuf, Vec<PathBuf>)> {
    use std::ffi::OsString;

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
fn links_to(raw: &str) -> PortResult<Vec<PathBuf>> {
    let (_, links) = follow(raw)?;
    links
        .iter()
        .map(|link| canonical_entry(path_str(link)?))
        .collect()
}

/// `path` with its deepest existing ancestor made canonical and the rest
/// appended. `raw` is what the caller asked about, for the error.
fn existing_ancestor_resolved(path: &Path, raw: &str) -> PortResult<PathBuf> {
    let mut missing: Vec<&std::ffi::OsStr> = Vec::new();
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
fn canonical_entry(raw: &str) -> PortResult<PathBuf> {
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
fn entries_to(path: &Path) -> PortResult<Vec<PathBuf>> {
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

/// `path` as an SBPL string literal.
fn sbpl_string(path: &Path) -> PortResult<String> {
    Ok(format!("\"{}\"", path_str(path)?.replace('\\', "\\\\")))
}

/// `pattern` as an SBPL regex literal (`#"..."`, where backslashes are kept
/// as written). The pattern is built from [`regex_escape`]d paths.
fn sbpl_regex(pattern: &str) -> PortResult<String> {
    if pattern.chars().any(|c| c == '"' || c.is_control()) {
        return Err(PortError::Invalid(
            "sandbox pattern contains a quote or control character".to_string(),
        ));
    }
    Ok(format!("#\"{pattern}\""))
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

/// Per-UID `.TemporaryItems` dir on macOS secondary volumes (required by Unity/Carbon).
pub(crate) fn secondary_volume_temp_dir(worktree: &Path, home: Option<&str>) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(worktree).unwrap_or_else(|_| worktree.to_path_buf());
    let uid = owner_uid(worktree).or_else(|| home.and_then(|h| owner_uid(Path::new(h))));
    secondary_volume_temp_dir_in(&canonical, uid, Path::exists)
}

#[cfg(unix)]
fn owner_uid(path: &Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|meta| meta.uid())
}

#[cfg(not(unix))]
fn owner_uid(_path: &Path) -> Option<u32> {
    None
}

pub(crate) fn secondary_volume_temp_dir_in(
    canonical: &Path,
    uid: Option<u32>,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    use std::ffi::OsStr;
    let mut comps = canonical.components();
    if comps.next() != Some(Component::RootDir)
        || comps.next() != Some(Component::Normal(OsStr::new("Volumes")))
    {
        return None;
    }
    let Component::Normal(vol) = comps.next()? else {
        return None;
    };
    if vol.is_empty() {
        return None;
    }
    let base = PathBuf::from("/Volumes").join(vol).join(".TemporaryItems");
    if let Some(uid) = uid {
        let per_uid = base.join(format!("folders.{uid}"));
        if exists(&per_uid) {
            return Some(per_uid);
        }
    }
    exists(&base).then_some(base)
}
