//! The `aaf-hook` entry in `.claude/settings.json`: merging it in for
//! `aaf init`, finding it again for `aaf doctor`, and refusing to start an
//! agent in a worktree that does not have it. One implementation for all
//! three, so they cannot disagree about what "registered" means.
//!
//! "Registered" is judged against an agent that can write every file of its
//! worktree: the entry must be able to block (nothing in the file takes its
//! verdict away), the program must really start, and a task worktree must
//! still have the file its base branch has.

mod check;
pub mod multi_host;
mod multi_host_specs;
mod validate;

use std::path::Path;

pub use check::{
    check_base, check_local, check_worktree, check_worktree_for_executor, resolve_program,
    strip_empty_hooks_object, validate_for_executor,
};
use serde_json::{Map, Value, json};
pub use validate::{command_program, sole_program};
use validate::{entry_matcher, entry_problem, forbidden_env, is_ours};

/// Tools whose calls the hook judges (PLAN C.3).
pub const HOOK_TOOLS: [&str; 5] = ["Bash", "Write", "Edit", "MultiEdit", "Read"];
pub const HOOK_BINARY: &str = "aaf-hook";

pub fn matcher() -> String {
    HOOK_TOOLS.join("|")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookChange {
    Added,
    /// An `aaf-hook` entry existed with another path or matcher.
    Updated,
    Unchanged,
}

impl HookChange {
    pub fn as_str(self) -> &'static str {
        match self {
            HookChange::Added => "added",
            HookChange::Updated => "updated",
            HookChange::Unchanged => "unchanged",
        }
    }
}

/// Repo-relative path of the settings file the hook is registered in.
pub const SETTINGS_REL: &str = ".claude/settings.json";

/// Repo-relative path of the settings file Claude Code reads on top of
/// [`SETTINGS_REL`]; its keys win.
pub const LOCAL_SETTINGS_REL: &str = ".claude/settings.local.json";

/// What git has of the settings file. A task worktree of `aaf run` is cut
/// from a commit: only a committed registration reaches the agents there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tracking {
    /// Tracked, and the working tree has what HEAD has.
    Committed,
    /// Tracked or staged, but not what HEAD has.
    Modified,
    Untracked,
    Ignored,
    /// git could not be asked.
    Unknown,
}

impl Tracking {
    /// From the trimmed output of `git status --porcelain --ignored -- <file>`
    /// for a file that exists.
    pub fn from_status(status: &str) -> Tracking {
        match status.lines().next().unwrap_or("") {
            "" => Tracking::Committed,
            line if line.starts_with("??") => Tracking::Untracked,
            line if line.starts_with("!!") => Tracking::Ignored,
            _ => Tracking::Modified,
        }
    }

    pub fn of(repo_root: &Path) -> Tracking {
        let args = ["status", "--porcelain", "--ignored", "--", SETTINGS_REL];
        match aaf_fp::git(repo_root, &args, &[], crate::gate::GIT_TIMEOUT) {
            Ok(status) => Tracking::from_status(&status),
            Err(_) => Tracking::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Tracking::Committed => "committed",
            Tracking::Modified => "modified",
            Tracking::Untracked => "untracked",
            Tracking::Ignored => "ignored",
            Tracking::Unknown => "unknown",
        }
    }

    /// Why the agents of `aaf run` do not get this registration, or `None`
    /// when they do.
    pub fn problem(self) -> Option<&'static str> {
        match self {
            Tracking::Committed => None,
            Tracking::Modified => Some("differs from HEAD"),
            Tracking::Untracked => Some("is untracked"),
            Tracking::Ignored => Some("is ignored by git"),
            Tracking::Unknown => Some("could not be compared with HEAD"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registered {
    pub matcher: String,
    pub command: String,
}

/// Claude Code runs the hook command through a shell, so a path with a space
/// or a metacharacter has to be quoted.
pub fn shell_quote(path: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    if !path.is_empty() && path.chars().all(plain) {
        path.to_string()
    } else {
        format!("'{}'", path.replace('\'', "'\\''"))
    }
}

fn parse(text: &str) -> Result<Value, String> {
    // An empty file is what a careless `touch` leaves; nothing is lost by
    // treating it as an empty object.
    if text.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let root: Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON ({e})"))?;
    if root.is_object() {
        Ok(root)
    } else {
        Err("the top level is not a JSON object".to_string())
    }
}

struct HookEntry {
    registered: Registered,
    matcher_valid: bool,
    problem: Option<String>,
}

/// Every `aaf-hook` entry under `hooks.PreToolUse`, each with what keeps it
/// from blocking (`None`: nothing).
fn entries(root: &Value) -> Vec<HookEntry> {
    let entries = root
        .get("hooks")
        .and_then(|hooks| hooks.get("PreToolUse"))
        .and_then(Value::as_array);
    let mut found = Vec::new();
    for entry in entries.into_iter().flatten() {
        let (matcher, matcher_valid, matcher_problem) = match entry_matcher(entry) {
            Ok(m) => (m.to_string(), true, None),
            Err(problem) => (
                entry
                    .get("matcher")
                    .map_or_else(String::new, Value::to_string),
                false,
                Some(problem),
            ),
        };
        let hooks = entry.get("hooks").and_then(Value::as_array);
        for hook in hooks.into_iter().flatten().filter(|hook| is_ours(hook)) {
            let registered = Registered {
                matcher: matcher.clone(),
                command: hook
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            };
            found.push(HookEntry {
                registered,
                matcher_valid,
                problem: matcher_problem.clone().or_else(|| entry_problem(hook)),
            });
        }
    }
    found
}

/// The registered `aaf-hook` entries of a settings file, for `aaf doctor`.
pub fn find_hooks(text: &str) -> Result<Vec<Registered>, String> {
    parse(text).map(|root| {
        entries(&root)
            .into_iter()
            .filter(|entry| entry.matcher_valid)
            .map(|entry| entry.registered)
            .collect()
    })
}

/// True when `matcher` sends every tool in [`HOOK_TOOLS`] to the hook.
pub fn matcher_covers_all(matcher: &str) -> bool {
    // Claude Code treats an empty matcher and `*` as "every tool".
    if matcher.is_empty() || matcher == "*" {
        return true;
    }
    let listed: Vec<&str> = matcher.split('|').map(str::trim).collect();
    HOOK_TOOLS.iter().all(|tool| listed.contains(tool))
}

/// `disableAllHooks` switches every hook off whatever is registered. Only
/// its absence or a literal `false` leaves them on; any other value fails
/// closed.
fn hooks_switched_off(root: &Value) -> Option<String> {
    match root.get("disableAllHooks") {
        None | Some(Value::Bool(false)) => None,
        Some(value) => Some(format!(
            "\"disableAllHooks\": {value} switches every hook off"
        )),
    }
}

/// The registration that sends every tool in [`HOOK_TOOLS`] to `aaf-hook`
/// and can block them, or what is wrong with the settings in `text`.
pub fn covering_hook(text: &str) -> Result<Registered, String> {
    let root = parse(text)?;
    if let Some(problem) = hooks_switched_off(&root) {
        return Err(problem);
    }
    let hooks = entries(&root);
    let mut covering = hooks
        .iter()
        .filter(|e| e.matcher_valid && matcher_covers_all(&e.registered.matcher));
    if let Some(entry) = covering.clone().find(|e| e.problem.is_none()) {
        if let Some(problem) = forbidden_env(&root) {
            return Err(problem);
        }
        return Ok(entry.registered.clone());
    }
    if let Some(problem) = covering.find_map(|e| e.problem.as_ref()) {
        return Err(problem.clone());
    }
    if let Some(problem) = hooks
        .iter()
        .filter(|e| !e.matcher_valid)
        .find_map(|e| e.problem.as_ref())
    {
        return Err(problem.clone());
    }
    Err(match hooks.first() {
        Some(entry) => {
            format!(
                "matcher {:?} does not cover {}",
                entry.registered.matcher,
                matcher()
            )
        }
        None => format!("no PreToolUse entry runs {HOOK_BINARY}"),
    })
}

/// Adds the hook entry running `command` to the settings in `existing`
/// (`None` when the file does not exist). Returns the new file text, or
/// `None` when the file already says exactly this. Every other key and hook
/// is kept. A file that is not the expected shape is an error, never
/// rewritten.
pub fn merge_hook(
    existing: Option<&str>,
    command: &str,
) -> Result<(Option<String>, HookChange), String> {
    let mut root = parse(existing.unwrap_or(""))?;
    let wanted = Registered {
        matcher: matcher(),
        command: command.to_string(),
    };
    let current = entries(&root);
    // An entry with a key that keeps it from blocking is rewritten too.
    if existing.is_some()
        && matches!(
            current.as_slice(),
            [only] if only.matcher_valid && only.problem.is_none() && only.registered == wanted
        )
    {
        return Ok((None, HookChange::Unchanged));
    }
    let change = if current.is_empty() {
        HookChange::Added
    } else {
        HookChange::Updated
    };

    let hooks = root
        .as_object_mut()
        .ok_or("the top level is not a JSON object")?
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or("\"hooks\" is not an object")?;
    let entries = hooks
        .entry("PreToolUse")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or("\"hooks.PreToolUse\" is not an array")?;
    // Drop stale `aaf-hook` registrations (old path, narrower matcher) but
    // leave any foreign hook that shares their entry where it is.
    entries.retain_mut(|entry| {
        let Some(list) = entry.get_mut("hooks").and_then(Value::as_array_mut) else {
            return true;
        };
        let before = list.len();
        list.retain(|hook| !is_ours(hook));
        // An entry that held nothing but our hook goes with it.
        list.len() == before || !list.is_empty()
    });
    entries.push(json!({
        "matcher": wanted.matcher,
        "hooks": [{"type": "command", "command": wanted.command}],
    }));

    let mut text = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    text.push('\n');
    Ok((Some(text), change))
}
