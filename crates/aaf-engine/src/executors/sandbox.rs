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
pub use crate::confine_core::path::canonical;
pub(crate) use crate::confine_core::path::{canonical_entry, entries_to, links_to, regex_escape};

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
    /// Additional task-worktree roots (e.g. default `~/.cache/autofactory/worktrees`
    /// or a previous `AAF_WORKTREE_DIR` when reusing a relocated worktree) whose
    /// sibling worktrees must also remain closed.
    pub extra_worktrees_roots: Vec<String>,
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
    let mut worktrees_roots = canonical_paths(
        extra
            .worktrees_root
            .iter()
            .chain(&extra.extra_worktrees_roots),
    )?;
    worktrees_roots.sort();
    worktrees_roots.dedup();
    let deny_keychain = extra
        .deny_keychain_dir
        .as_deref()
        .map(canonical)
        .transpose()?;
    if !worktrees_roots.is_empty() {
        // Another task's worktree is never writable, whoever listed it.
        writable.retain(|path| !worktrees_roots.iter().any(|root| path.starts_with(root)));
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
    // sibling task worktrees: close every worktrees root, and reopen only this
    // run's own worktree when it lies below one of them. A worktree that is
    // not below any root is opened first, so it cannot re-open a root.
    let own_is_task_worktree = worktree.as_ref().is_some_and(|own| {
        worktrees_roots
            .iter()
            .any(|root| own != root && own.starts_with(root))
    });
    let open_own = match &worktree {
        Some(own) => format!("(allow file-write* (subpath {}))\n", sbpl_string(own)?),
        None => String::new(),
    };
    if !own_is_task_worktree {
        profile.push_str(&open_own);
    }
    for root in &worktrees_roots {
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
    let (mut submodules, run_worktree) = (Vec::new(), worktree.as_deref());
    for git_dir in &git_dirs {
        let subs = git_dir::submodule_git_dirs(git_dir)?;
        profile.push_str(&git_dir::rules(git_dir, &subs)?);
        profile.push_str(&git_dir::linked_worktree_rules(git_dir, run_worktree)?);
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
    closed.extend(worktrees_roots.clone());
    // The obeyed files of every git dir, submodules included: renaming a
    // directory on the way to one would carry it out of its rules (F7-R4-2).
    for root in git_dirs.iter().chain(&submodules) {
        closed.extend(git_dir::OBEYED.iter().map(|name| root.join(name)));
    }
    for git_dir in &git_dirs {
        closed.extend(git_dir::protected_closed_paths(git_dir, run_worktree));
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
