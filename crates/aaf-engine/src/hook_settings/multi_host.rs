//! Multi-host hook registration and verification for Codex CLI, Gemini CLI,
//! Antigravity, and OpenCode.
//!
//! Mirrors the Claude Code `.claude/settings.json` contract from
//! `super` so `aaf init --hook-host ...`, `aaf doctor`, and `aaf run` share one
//! strict definition of "registered" across all supported coding-agent hosts.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

static WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

use super::multi_host_specs::{ANTIGRAVITY_SPEC, CODEX_SPEC, GEMINI_SPEC, HostSpec, OPENCODE_SPEC};
pub use super::multi_host_specs::{
    ANTIGRAVITY_TOOLS, CODEX_HOOKS_REL, CODEX_TOOLS, GEMINI_SETTINGS_REL, GEMINI_TOOLS,
    HookHostTarget, HostHookStatus, OPENCODE_HOOKS_REL, OPENCODE_TOOLS, antigravity_matcher,
    codex_matcher, gemini_matcher, opencode_matcher, parse_host_arg,
};
use super::{
    HOOK_BINARY, HookChange, HookEntry, Registered, covering_hook, entry_matcher, forbidden_env,
    hooks_switched_off, is_ours, parse, shell_quote,
};

fn format_host_command(command_path: &str, host_flag: &str) -> Result<String, String> {
    let trimmed = command_path.trim();
    if trimmed.is_empty() {
        return Err(format!("{HOOK_BINARY} command path must not be empty"));
    }
    Ok(format!("{} --host {host_flag}", shell_quote(trimmed)))
}

/// Parses a host hook command string and ensures it runs ONLY `aaf-hook` with
/// `--host <expected_host>` (or `--host=<expected_host>`) and zero shell
/// operators, redirections, substitutions, env assignments, or extra flags.
///
/// Returns the unquoted program path on success.
fn parse_host_command(command: &str, expected_host: &str) -> Result<String, String> {
    let reject = || {
        format!(
            "the command {command:?} must invoke only {HOOK_BINARY} with `--host {expected_host}` \
             (or `--host={expected_host}`), with no shell operator, redirection, or extra argument"
        )
    };
    if command.contains('\n') || command.contains('\r') {
        return Err(reject());
    }
    let raw = command.trim_matches([' ', '\t']);
    let plain = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    // Reject `NAME=value ...` env-override prefixes.
    let ident_len = raw
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .count();
    if ident_len > 0 && raw[ident_len..].starts_with('=') {
        return Err(reject());
    }

    let mut program = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => break,
            '\'' => loop {
                match chars.next() {
                    Some('\'') => break,
                    Some(quoted) => program.push(quoted),
                    None => return Err(reject()),
                }
            },
            '"' => loop {
                match chars.next() {
                    Some('"') => break,
                    Some(quoted) if !"$`\\".contains(quoted) => program.push(quoted),
                    _ => return Err(reject()),
                }
            },
            '\\' => match chars.next() {
                Some('\'') => program.push('\''),
                _ => return Err(reject()),
            },
            c if plain(c) => program.push(c),
            _ => return Err(reject()),
        }
    }
    if program.is_empty()
        || Path::new(&program)
            .file_name()
            .is_none_or(|name| name != OsStr::new(HOOK_BINARY))
    {
        return Err(reject());
    }

    let rest: String = chars.collect();
    let tail = rest.trim_matches([' ', '\t']);
    let eq_form = format!("--host={expected_host}");
    if tail == eq_form {
        return Ok(program);
    }
    let mut parts = tail.split([' ', '\t']).filter(|s| !s.is_empty());
    match (parts.next(), parts.next(), parts.next()) {
        (Some("--host"), Some(host), None) if host == expected_host => Ok(program),
        _ => Err(reject()),
    }
}

fn host_entry_problem(hook: &Value, spec: HostSpec) -> Option<String> {
    let Some(map) = hook.as_object() else {
        return Some(format!("the {HOOK_BINARY} entry is not a JSON object"));
    };
    let mut has_type = false;
    for (key, value) in map {
        match key.as_str() {
            "command" => {}
            "type" if value == "command" => has_type = true,
            "timeout" if value.as_u64().is_some_and(|val| val >= spec.min_timeout) => {}
            "name" | "description" | "statusMessage" if value.is_string() => {}
            other => {
                return Some(format!(
                    "the {HOOK_BINARY} entry sets \"{other}\": {value}, which a plain blocking \
                     command hook does not have (allowed: \"type\": \"command\", \"command\", \
                     an integer \"timeout\" of at least {} {}, or none)",
                    spec.min_timeout, spec.timeout_unit
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
    parse_host_command(command, spec.host_flag).err()
}

fn matcher_covers_spec(matcher: &str, spec: HostSpec) -> bool {
    if matcher.is_empty() || matcher == "*" || matcher == ".*" {
        return true;
    }
    let listed: Vec<&str> = matcher.split('|').map(str::trim).collect();
    spec.tools.iter().all(|tool| listed.contains(tool))
}

fn host_entries(root: &Value, spec: HostSpec) -> Vec<HookEntry> {
    let entries = root
        .get("hooks")
        .and_then(|hooks| hooks.get(spec.event_key))
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
                problem: matcher_problem
                    .clone()
                    .or_else(|| host_entry_problem(hook, spec)),
            });
        }
    }
    found
}

fn gemini_hooks_disabled(root: &Value) -> Option<String> {
    let config = root.get("hooksConfig")?;
    let Some(obj) = config.as_object() else {
        return Some(format!("\"hooksConfig\": {config} is not a JSON object"));
    };
    let enabled = obj.get("enabled")?;
    match enabled {
        Value::Bool(true) => None,
        other => Some(format!(
            "\"hooksConfig.enabled\": {other} switches Gemini hooks off"
        )),
    }
}

fn covering_host_hook(text: &str, spec: HostSpec) -> Result<Registered, String> {
    let root = parse(text)?;
    if let Some(problem) = hooks_switched_off(&root) {
        return Err(problem);
    }
    if spec.check_gemini_hooks_config
        && let Some(problem) = gemini_hooks_disabled(&root)
    {
        return Err(problem);
    }
    let hooks = host_entries(&root, spec);
    let mut covering = hooks
        .iter()
        .filter(|e| e.matcher_valid && matcher_covers_spec(&e.registered.matcher, spec));
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
        Some(entry) => format!(
            "matcher {:?} does not cover {}",
            entry.registered.matcher,
            spec.tools.join("|")
        ),
        None => format!("no {} entry runs {HOOK_BINARY}", spec.event_key),
    })
}

fn merge_host_hook(
    existing: Option<&str>,
    command_path: &str,
    spec: HostSpec,
) -> Result<(Option<String>, HookChange), String> {
    let mut root = parse(existing.unwrap_or(""))?;
    let wanted_cmd = format_host_command(command_path, spec.host_flag)?;
    let wanted = Registered {
        matcher: spec.tools.join("|"),
        command: wanted_cmd,
    };
    let current = host_entries(&root, spec);
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
        .entry(spec.event_key)
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| format!("\"hooks.{}\" is not an array", spec.event_key))?;

    entries.retain_mut(|entry| {
        let Some(list) = entry.get_mut("hooks").and_then(Value::as_array_mut) else {
            return true;
        };
        let before = list.len();
        list.retain(|hook| !is_ours(hook));
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

fn register_host(
    repo_root: &Path,
    command_path: &str,
    spec: HostSpec,
) -> Result<HookChange, String> {
    let path = repo_root.join(spec.settings_rel);
    let existing = match fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => return Err(format!("cannot read {}: {err}", path.display())),
    };
    let (updated, change) = merge_host_hook(existing.as_deref(), command_path, spec)?;
    if let Some(text) = updated {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("cannot create {}: {err}", parent.display()))?;
        }
        let seq = WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
        let mut tmp_name = path
            .file_name()
            .map(OsStr::to_os_string)
            .unwrap_or_default();
        tmp_name.push(format!(".aaf-tmp-{}-{seq}", std::process::id()));
        let tmp = path.with_file_name(tmp_name);
        if let Err(err) = fs::write(&tmp, text) {
            let _ = fs::remove_file(&tmp);
            return Err(format!("cannot write {}: {err}", path.display()));
        }
        if let Err(err) = fs::rename(&tmp, &path) {
            let _ = fs::remove_file(&tmp);
            return Err(format!("cannot write {}: {err}", path.display()));
        }
    }
    Ok(change)
}

fn check_host(repo_root: &Path, spec: HostSpec) -> Result<Registered, String> {
    let path = repo_root.join(spec.settings_rel);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Err(format!("{} does not exist", spec.settings_rel));
        }
        Err(err) => {
            return Err(format!("{} cannot be read ({err})", spec.settings_rel));
        }
    };
    covering_host_hook(&text, spec)
}

/// Pure merge for `.codex/hooks.json`.
pub fn merge_codex_hook(
    existing: Option<&str>,
    command_path: &str,
) -> Result<(Option<String>, HookChange), String> {
    merge_host_hook(existing, command_path, CODEX_SPEC)
}

/// Pure merge for `.gemini/settings.json` (`--host gemini`).
pub fn merge_gemini_hook(
    existing: Option<&str>,
    command_path: &str,
) -> Result<(Option<String>, HookChange), String> {
    merge_host_hook(existing, command_path, GEMINI_SPEC)
}

/// Pure merge for `.gemini/settings.json` (`--host antigravity`).
pub fn merge_antigravity_hook(
    existing: Option<&str>,
    command_path: &str,
) -> Result<(Option<String>, HookChange), String> {
    merge_host_hook(existing, command_path, ANTIGRAVITY_SPEC)
}

/// Pure merge for `.opencode/hooks.json` (`--host opencode`).
pub fn merge_opencode_hook(
    existing: Option<&str>,
    command_path: &str,
) -> Result<(Option<String>, HookChange), String> {
    merge_host_hook(existing, command_path, OPENCODE_SPEC)
}

/// Validates the Codex CLI hook registration text in `.codex/hooks.json`.
pub fn covering_codex_hook(text: &str) -> Result<Registered, String> {
    covering_host_hook(text, CODEX_SPEC)
}

/// Validates the Gemini CLI hook registration text in `.gemini/settings.json`.
pub fn covering_gemini_hook(text: &str) -> Result<Registered, String> {
    covering_host_hook(text, GEMINI_SPEC)
}

/// Validates the Antigravity hook registration text in `.gemini/settings.json`.
pub fn covering_antigravity_hook(text: &str) -> Result<Registered, String> {
    covering_host_hook(text, ANTIGRAVITY_SPEC)
}

/// Validates the OpenCode hook registration text in `.opencode/hooks.json`.
pub fn covering_opencode_hook(text: &str) -> Result<Registered, String> {
    covering_host_hook(text, OPENCODE_SPEC)
}

/// Registers (or updates) `aaf-hook --host codex` in `<repo_root>/.codex/hooks.json`.
pub fn register_codex(repo_root: &Path, command_path: &str) -> Result<HookChange, String> {
    register_host(repo_root, command_path, CODEX_SPEC)
}

/// Registers (or updates) `aaf-hook --host gemini` in `<repo_root>/.gemini/settings.json`.
pub fn register_gemini(repo_root: &Path, command_path: &str) -> Result<HookChange, String> {
    register_host(repo_root, command_path, GEMINI_SPEC)
}

/// Registers (or updates) `aaf-hook --host antigravity` in `<repo_root>/.gemini/settings.json`.
pub fn register_antigravity(repo_root: &Path, command_path: &str) -> Result<HookChange, String> {
    register_host(repo_root, command_path, ANTIGRAVITY_SPEC)
}

/// Registers (or updates) `aaf-hook --host opencode` in `<repo_root>/.opencode/hooks.json`.
pub fn register_opencode(repo_root: &Path, command_path: &str) -> Result<HookChange, String> {
    register_host(repo_root, command_path, OPENCODE_SPEC)
}

/// Verifies `<repo_root>/.codex/hooks.json` has an untampered `aaf-hook --host codex` entry.
pub fn check_codex(repo_root: &Path) -> Result<Registered, String> {
    check_host(repo_root, CODEX_SPEC)
}

/// Verifies `<repo_root>/.gemini/settings.json` has an untampered `aaf-hook --host gemini` entry.
pub fn check_gemini(repo_root: &Path) -> Result<Registered, String> {
    check_host(repo_root, GEMINI_SPEC)
}

/// Verifies `<repo_root>/.gemini/settings.json` has an untampered `aaf-hook --host antigravity` entry.
pub fn check_antigravity(repo_root: &Path) -> Result<Registered, String> {
    check_host(repo_root, ANTIGRAVITY_SPEC)
}

/// Verifies `<repo_root>/.opencode/hooks.json` has an untampered `aaf-hook --host opencode` entry.
pub fn check_opencode(repo_root: &Path) -> Result<Registered, String> {
    check_host(repo_root, OPENCODE_SPEC)
}

/// Checks a host's hook configuration if its settings file is present.
pub fn check_host_if_present(repo_root: &Path, host: HookHostTarget) -> HostHookStatus {
    let path = repo_root.join(host.settings_rel());
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return HostHookStatus::NotConfigured;
        }
        Err(err) => {
            return HostHookStatus::Invalid(format!(
                "{} cannot be read ({err})",
                host.settings_rel()
            ));
        }
    };
    let result = match host {
        HookHostTarget::Claude => covering_hook(&text),
        HookHostTarget::Codex => covering_codex_hook(&text),
        HookHostTarget::Gemini => covering_gemini_hook(&text),
        HookHostTarget::Antigravity => covering_antigravity_hook(&text),
        HookHostTarget::Opencode => covering_opencode_hook(&text),
    };
    match result {
        Ok(registered) => HostHookStatus::Registered(registered),
        Err(problem) => HostHookStatus::Invalid(problem),
    }
}
