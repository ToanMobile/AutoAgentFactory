//! The `aaf-hook` entry in `.claude/settings.json`: merging it in for
//! `aaf init`, finding it again for `aaf doctor`, and refusing to start an
//! agent in a worktree that does not have it. One implementation for all
//! three, so they cannot disagree about what "registered" means.
//!
//! "Registered" is judged against an agent that can write every file of its
//! worktree: the entry must be able to block (nothing in the file takes its
//! verdict away), the program must really start, and a task worktree must
//! still have the file its base branch has.

pub mod multi_host;
mod validate;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use validate::{entry_problem, forbidden_env};

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

/// The first shell word of `command` with its quoting removed. Only to
/// recognise an entry as ours (whatever follows the word); whether the
/// entry may stand is [`sole_program`]'s question.
pub fn command_program(command: &str) -> Option<String> {
    let mut out = String::new();
    let mut seen = false;
    let mut chars = command.trim_start().chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                seen = true;
                out.extend(chars.by_ref().take_while(|q| *q != '\''));
            }
            '"' => {
                seen = true;
                while let Some(q) = chars.next() {
                    match q {
                        '"' => break,
                        '\\' => out.extend(chars.next()),
                        _ => out.push(q),
                    }
                }
            }
            '\\' => {
                seen = true;
                out.extend(chars.next());
            }
            c if c.is_whitespace() => break,
            c => {
                seen = true;
                out.push(c);
            }
        }
    }
    seen.then_some(out)
}

/// The program `command` runs, when the command is that program and nothing
/// else: one name or path, optionally quoted. Claude Code hands the command
/// to a shell, so anything further (`|| true`, a redirection, a substitution,
/// an argument) can throw the hook's verdict away while the entry still
/// looks registered.
pub fn sole_program(command: &str) -> Result<String, String> {
    let reject = || {
        format!(
            "the command {command:?} must be only the program {HOOK_BINARY} (its name or an \
             absolute path, optionally quoted), with no argument, redirection or shell operator"
        )
    };
    let plain = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    let raw = command.trim();
    // `NAME=value` alone is an assignment: the shell runs nothing and exits 0.
    let name = raw
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .count();
    if name > 0 && raw[name..].starts_with('=') {
        return Err(reject());
    }
    let mut out = String::new();
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => loop {
                match chars.next() {
                    Some('\'') => break,
                    Some(quoted) => out.push(quoted),
                    None => return Err(reject()),
                }
            },
            '"' => loop {
                match chars.next() {
                    Some('"') => break,
                    // Expansions and escapes work inside double quotes.
                    Some(quoted) if !"$`\\".contains(quoted) => out.push(quoted),
                    _ => return Err(reject()),
                }
            },
            // Only `\'`, which is how `shell_quote` writes a quote.
            '\\' => match chars.next() {
                Some('\'') => out.push('\''),
                _ => return Err(reject()),
            },
            c if plain(c) => out.push(c),
            _ => return Err(reject()),
        }
    }
    if out.is_empty() {
        return Err(reject());
    }
    Ok(out)
}

fn is_ours(hook: &Value) -> bool {
    hook.get("command")
        .and_then(Value::as_str)
        .and_then(command_program)
        .is_some_and(|program| {
            Path::new(&program)
                .file_name()
                .is_some_and(|name| name == HOOK_BINARY)
        })
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

/// Every `aaf-hook` entry under `hooks.PreToolUse`, each with what keeps it
/// from blocking (`None`: nothing).
fn entries(root: &Value) -> Vec<(Registered, Option<String>)> {
    let entries = root
        .get("hooks")
        .and_then(|hooks| hooks.get("PreToolUse"))
        .and_then(Value::as_array);
    let mut found = Vec::new();
    for entry in entries.into_iter().flatten() {
        let matcher = entry.get("matcher").and_then(Value::as_str).unwrap_or("");
        let hooks = entry.get("hooks").and_then(Value::as_array);
        for hook in hooks.into_iter().flatten().filter(|hook| is_ours(hook)) {
            let registered = Registered {
                matcher: matcher.to_string(),
                command: hook
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            };
            found.push((registered, entry_problem(hook)));
        }
    }
    found
}

/// The registered `aaf-hook` entries of a settings file, for `aaf doctor`.
pub fn find_hooks(text: &str) -> Result<Vec<Registered>, String> {
    parse(text).map(|root| entries(&root).into_iter().map(|(hook, _)| hook).collect())
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
        .filter(|(hook, _)| matcher_covers_all(&hook.matcher));
    if let Some((hook, _)) = covering.clone().find(|(_, problem)| problem.is_none()) {
        if let Some(problem) = forbidden_env(&root, !hook.command.contains('/')) {
            return Err(problem);
        }
        return Ok(hook.clone());
    }
    Err(match (covering.next(), hooks.first()) {
        (Some((_, Some(problem))), _) => problem.clone(),
        (_, Some((hook, _))) => {
            format!("matcher {:?} does not cover {}", hook.matcher, matcher())
        }
        _ => format!("no PreToolUse entry runs {HOOK_BINARY}"),
    })
}

/// Why the local settings in `text` take the registration away again, or
/// `Ok`. A file that cannot be read as settings fails closed.
pub fn check_local(text: &str) -> Result<(), String> {
    let root = parse(text)?;
    for key in ["hooks", "disableAllHooks"] {
        if root.get(key).is_some() {
            return Err(format!(
                "it sets \"{key}\", which overrides the hook registration"
            ));
        }
    }
    if let Some(problem) = forbidden_env(&root, true) {
        return Err(format!("it sets {problem}"));
    }
    Ok(())
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The file the registered `program` runs for an agent whose PATH is
/// `path_var`, or why it would not run. A hook that cannot be started ends
/// with exit 127, and Claude Code treats only exit 2 as a block: the agent
/// would go on unguarded. `outside` is the agent's worktree: a program in
/// there is the agent's to replace.
pub fn resolve_program(
    program: &str,
    path_var: &OsStr,
    outside: Option<&Path>,
) -> Result<PathBuf, String> {
    let candidate = Path::new(program);
    let found = if candidate.is_absolute() {
        if !is_executable(candidate) {
            return Err(format!("{program} is missing or not executable"));
        }
        candidate.to_path_buf()
    } else if program.contains('/') {
        return Err(format!(
            "{program} is a relative path: it is looked up in the directory the agent works in"
        ));
    } else {
        std::env::split_paths(path_var)
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join(program))
            .find(|candidate| is_executable(candidate))
            .ok_or_else(|| {
                format!(
                    "{program} is not found on the PATH the agent gets ({})",
                    path_var.to_string_lossy()
                )
            })?
    };
    if let Some(root) = outside {
        let real = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if real(&found).starts_with(real(root)) {
            return Err(format!(
                "{} is inside the agent's worktree, where the agent can replace it",
                found.display()
            ));
        }
    }
    Ok(found)
}

/// What to do about a problem, as the end of its message.
enum Fix {
    Register,
    Path,
    Review,
}

impl Fix {
    fn text(&self) -> String {
        match self {
            Fix::Register => format!(
                "run `aaf init` in the repository and commit {SETTINGS_REL} on the base branch \
                 (git add {SETTINGS_REL} && git commit)"
            ),
            Fix::Path => format!(
                "start `aaf run` with the directory of {HOOK_BINARY} on PATH, or register its \
                 absolute path in {SETTINGS_REL} and commit that on the base branch"
            ),
            Fix::Review => format!(
                "the hook registration changes only on the base branch, by a person: look at \
                 what the task did to {SETTINGS_REL} and {LOCAL_SETTINGS_REL} in its worktree, \
                 then discard it or commit it on the base branch yourself"
            ),
        }
    }
}

/// The registration in `text` and the program it runs, checked together.
fn check_text(text: &str, path_var: &OsStr, outside: Option<&Path>) -> Result<(), (String, Fix)> {
    let hook = covering_hook(text).map_err(|problem| (problem, Fix::Register))?;
    let program = sole_program(&hook.command).map_err(|problem| (problem, Fix::Register))?;
    resolve_program(&program, path_var, outside)
        .map(|_| ())
        .map_err(|problem| (problem, Fix::Path))
}

fn check_dir(dir: &Path, path_var: &OsStr, base: Option<&str>) -> Result<(), (String, Fix)> {
    let text = match std::fs::read_to_string(dir.join(SETTINGS_REL)) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(("the file does not exist".to_string(), Fix::Register));
        }
        Err(err) => {
            return Err((format!("the file cannot be read ({err})"), Fix::Register));
        }
    };
    let valid = check_text(&text, path_var, Some(dir));
    if let Some(base) = base {
        // Where the task's branch left the base: an older registration is
        // still the base's own, a different one is the task's doing, and
        // that is said before anything else that is wrong with it.
        let git = |args: &[&str]| aaf_fp::git(dir, args, &[], crate::gate::GIT_TIMEOUT);
        let committed = git(&["merge-base", "HEAD", base])
            .and_then(|fork| git(&["show", &format!("{fork}:{SETTINGS_REL}")]));
        let changed = match committed {
            Ok(committed) if committed == text.trim() => None,
            Ok(_) => Some("the file is not what the base branch has: the task changed it".into()),
            Err(err) => Some(format!(
                "the file could not be compared with the base branch ({err})"
            )),
        };
        if let Some(mut problem) = changed {
            if let Err((inner, _)) = &valid {
                problem.push_str(&format!(" ({inner})"));
            }
            return Err((problem, Fix::Review));
        }
    }
    valid?;
    match std::fs::read_to_string(dir.join(LOCAL_SETTINGS_REL)) {
        Ok(local) => check_local(&local)
            .map_err(|problem| (format!("{LOCAL_SETTINGS_REL}: {problem}"), Fix::Review))?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            let problem = format!("{LOCAL_SETTINGS_REL} cannot be read ({err})");
            return Err((problem, Fix::Review));
        }
    }
    Ok(())
}

/// Why an agent started in `worktree` with `path_var` as its PATH would run
/// without the hook, or `Ok`.
///
/// A task worktree is cut from a commit, so what counts is the settings
/// file as it is in that worktree, not the one in the main checkout. With
/// `base` (the tip of the base branch) the file must also be the one the
/// base branch had where the task's branch left it: a task never changes
/// its own guard. The message ends with what to do about it.
pub fn check_worktree(worktree: &Path, path_var: &OsStr, base: Option<&str>) -> Result<(), String> {
    check_dir(worktree, path_var, base).map_err(|(problem, fix)| {
        format!(
            "the agent would run without {HOOK_BINARY}: {} in the task worktree: {problem} — \
             fix: {}, then `aaf board unblock` the task",
            worktree.join(SETTINGS_REL).display(),
            fix.text()
        )
    })
}

/// Why the agents of an `aaf run` on `repo_root` would run without the
/// hook, judged by the settings file committed at `base` (what every task
/// worktree is cut from), or `Ok`. Asked once before any task is taken: a
/// registration that is wrong on the base is wrong for every task alike.
pub fn check_base(repo_root: &Path, base: &str, path_var: &OsStr) -> Result<(), String> {
    let spec = format!("{base}:{SETTINGS_REL}");
    let result = match aaf_fp::git(repo_root, &["show", &spec], &[], crate::gate::GIT_TIMEOUT) {
        Ok(text) => check_text(&text, path_var, None),
        Err(_) => Err(("the file is not committed there".to_string(), Fix::Register)),
    };
    result.map_err(|(problem, fix)| {
        format!(
            "the agents of this run would start without {HOOK_BINARY}: {SETTINGS_REL} on the \
             base branch: {problem} — fix: {}, then start `aaf run` again; no task was touched",
            fix.text()
        )
    })?;
    let unreadable = |err: aaf_fp::FpError| {
        format!(
            "the agents of this run would start without {HOOK_BINARY}: {LOCAL_SETTINGS_REL} \
             on the base branch could not be read ({err}); no task was touched"
        )
    };
    // `ls-tree` reads trees only and says "absent" with an empty success;
    // `cat-file -e` fails alike for an absent file, a missing object and a
    // git that did not answer, and only the first may skip the check.
    let listed = aaf_fp::git(
        repo_root,
        &["ls-tree", "--name-only", base, "--", LOCAL_SETTINGS_REL],
        &[],
        crate::gate::GIT_TIMEOUT,
    )
    .map_err(unreadable)?;
    if !listed.is_empty() {
        let local_spec = format!("{base}:{LOCAL_SETTINGS_REL}");
        let local_text = aaf_fp::git(
            repo_root,
            &["show", &local_spec],
            &[],
            crate::gate::GIT_TIMEOUT,
        )
        .map_err(unreadable)?;
        if let Err(problem) = check_local(&local_text) {
            return Err(format!(
                "the agents of this run would start without {HOOK_BINARY}: {LOCAL_SETTINGS_REL} \
                 on the base branch: {problem} — fix: {}, then start `aaf run` again; no task \
                 was touched",
                Fix::Review.text()
            ));
        }
    }
    Ok(())
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
    if existing.is_some() && current == [(wanted.clone(), None)] {
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
