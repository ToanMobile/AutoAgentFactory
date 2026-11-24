//! Validation of individual `aaf-hook` entries, commands, matchers, and
//! settings environment keys.

use std::path::Path;

use serde_json::Value;

use super::HOOK_BINARY;

/// Shortest `timeout` an `aaf-hook` entry may set. A hook that runs out of
/// time lets the tool call through unguarded, and the hook may wait on git
/// for up to `aaf_fp::GIT_TIMEOUT` (10 s) more than once. `aaf init` sets
/// no timeout, which leaves Claude Code's own default (longer than this).
const MIN_HOOK_TIMEOUT_SECS: u64 = 30;

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

pub(super) fn is_ours(hook: &Value) -> bool {
    hook.get("command")
        .and_then(Value::as_str)
        .and_then(command_program)
        .is_some_and(|program| {
            Path::new(&program)
                .file_name()
                .is_some_and(|name| name == HOOK_BINARY)
        })
}

/// Reads the optional `"matcher"` field of a hook group entry.
///
/// Absent `"matcher"` means wildcard (`""`); any present non-string value
/// (`null`, bool, number, array, object) is rejected instead of collapsing to
/// `""`.
pub(super) fn entry_matcher(entry: &Value) -> Result<&str, String> {
    match entry.get("matcher") {
        None => Ok(""),
        Some(Value::String(s)) => Ok(s.as_str()),
        Some(other) => Err(format!("\"matcher\": {other} is not a string")),
    }
}

/// Why an `aaf-hook` entry cannot block a tool call although it is listed:
/// anything but a plain command hook (`async`, `if`, another or missing `type`,
/// a `timeout` that is not an integer of at least [`MIN_HOOK_TIMEOUT_SECS`])
/// or a command that is more than the program.
pub(super) fn entry_problem(hook: &Value) -> Option<String> {
    let map = hook.as_object()?;
    let mut has_type = false;
    for (key, value) in map {
        match key.as_str() {
            "command" => {}
            "type" if value == "command" => has_type = true,
            "timeout"
                if value
                    .as_u64()
                    .is_some_and(|secs| secs >= MIN_HOOK_TIMEOUT_SECS) => {}
            other => {
                return Some(format!(
                    "the {HOOK_BINARY} entry sets \"{other}\": {value}, which a plain blocking \
                     command hook does not have (allowed: \"type\": \"command\", \"command\", \
                     an integer \"timeout\" of at least {MIN_HOOK_TIMEOUT_SECS} seconds, or none)"
                ));
            }
        }
    }
    if !has_type {
        return Some(format!(
            "the {HOOK_BINARY} entry is missing \"type\": \"command\""
        ));
    }
    let command = map.get("command").and_then(Value::as_str).unwrap_or("");
    sole_program(command).err()
}

/// True when `name` is an environment variable that redirects `aaf-hook`'s
/// data/config/git lookups, strips its run marker, or injects code into it.
/// Every `GIT_*` but the author and committer identity: `aaf_fp::git` drops
/// only `GIT_DIR`, `GIT_WORK_TREE` and `GIT_INDEX_FILE`, and the others move
/// where git looks (`GIT_CEILING_DIRECTORIES` hides the repository from a
/// directory inside it) or what it reads (`GIT_OBJECT_DIRECTORY`,
/// `GIT_ALTERNATE_OBJECT_DIRECTORIES`, `GIT_NAMESPACE`).
fn is_guard_env(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    let git = upper.starts_with("GIT_")
        && !upper.starts_with("GIT_AUTHOR_")
        && !upper.starts_with("GIT_COMMITTER_");
    git || matches!(
        upper.as_str(),
        "HOME" | "XDG_CONFIG_HOME" | "SSH_ASKPASS" | "BASH_ENV" | "ENV" | "ZDOTDIR"
    ) || upper.starts_with("AAF_")
        || upper.starts_with("DYLD_")
        || upper.starts_with("LD_")
}

/// Why `"env"` in a host settings file would subvert `aaf-hook`.
///
/// `"env"."PATH"` is always forbidden — even when the hook is registered by
/// absolute path — because `PATH` controls both hook helper lookups (`git`)
/// and commands run inside the host session.
pub(super) fn forbidden_env(root: &Value) -> Option<String> {
    let env = root.get("env")?;
    let Some(map) = env.as_object() else {
        return Some("\"env\" is not a JSON object".to_string());
    };
    if let Some(key) = map.keys().find(|key| key.eq_ignore_ascii_case("PATH")) {
        return Some(format!(
            "\"env\".\"{key}\" is set, which overrides PATH for {HOOK_BINARY} and its helpers"
        ));
    }
    map.keys()
        .find(|key| is_guard_env(key))
        .map(|key| format!("\"env\".\"{key}\" overrides the environment {HOOK_BINARY} trusts"))
}
