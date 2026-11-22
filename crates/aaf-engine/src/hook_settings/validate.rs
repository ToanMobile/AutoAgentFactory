//! Validation of individual `aaf-hook` entries and settings environment keys.

use serde_json::Value;

use super::{HOOK_BINARY, sole_program};

/// Shortest `timeout` an `aaf-hook` entry may set. A hook that runs out of
/// time lets the tool call through unguarded, and the hook may wait on git
/// for up to `aaf_fp::GIT_TIMEOUT` (10 s) more than once. `aaf init` sets
/// no timeout, which leaves Claude Code's own default (longer than this).
const MIN_HOOK_TIMEOUT_SECS: u64 = 30;

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

/// Why `"env"` in a Claude Code settings file would subvert `aaf-hook`.
/// `forbid_path` is true for local settings and whenever the hook is registered
/// by bare name.
pub(super) fn forbidden_env(root: &Value, forbid_path: bool) -> Option<String> {
    let env = root.get("env")?;
    let Some(map) = env.as_object() else {
        return Some("\"env\" is not a JSON object".to_string());
    };
    if forbid_path && map.contains_key("PATH") {
        return Some(format!(
            "\"env\".\"PATH\" is set, which decides what the name {HOOK_BINARY} runs"
        ));
    }
    map.keys()
        .find(|key| is_guard_env(key))
        .map(|key| format!("\"env\".\"{key}\" overrides the environment {HOOK_BINARY} trusts"))
}
