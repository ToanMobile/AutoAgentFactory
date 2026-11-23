//! Multi-host hook payload adapters (Claude Code, Codex CLI, Gemini CLI).
//!
//! Normalizes tool-call payloads from all three supported coding-agent hosts
//! into a single [`ToolAction`] so every host runs through the exact same
//! guard suite, and renders the host-specific allow/deny JSON response.

use std::fs;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;

use serde_json::{Map, Value, json};

use crate::{Ctx, Decision, Denial, HookEnv, Outcome};

const INPUT_RULE: &str = "hook_input";
const INPUT_FIX: &str = "send a valid PreToolUse or BeforeTool JSON payload on stdin";

/// Coding-agent host invoking `aaf-hook`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookHost {
    Auto,
    Claude,
    Codex,
    Gemini,
}

impl HookHost {
    /// Parses a `--host` flag value (`auto`, `claude`, `codex`, `gemini`).
    pub fn parse_arg(value: &str) -> Option<HookHost> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(HookHost::Auto),
            "claude" => Some(HookHost::Claude),
            "codex" => Some(HookHost::Codex),
            "gemini" => Some(HookHost::Gemini),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            HookHost::Auto => "auto",
            HookHost::Claude => "claude",
            HookHost::Codex => "codex",
            HookHost::Gemini => "gemini",
        }
    }
}

/// Host-agnostic tool action extracted from a hook payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolAction {
    Shell(String),
    FileRead(String),
    FileReadMany(Vec<String>),
    FileWrite(Vec<String>),
    Unknown,
}

/// Parsed hook payload with its resolved host, working directory, and action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCall {
    pub resolved_host: HookHost,
    pub cwd: Option<String>,
    pub action: ToolAction,
}

fn bad_input(reason: impl Into<String>) -> Denial {
    Denial::new(INPUT_RULE, reason, INPUT_FIX)
}

/// Parses CLI arguments after `argv[0]` (e.g., `env::args().skip(1)`).
///
/// Accepts no arguments (defaults to [`HookHost::Auto`]),
/// `--host <auto|claude|codex|gemini>`, or `--host=<value>`.
/// Fails closed on any unknown flag, missing value, or extra argument.
pub fn parse_cli_host(args: &[String]) -> Result<HookHost, String> {
    match args {
        [] => Ok(HookHost::Auto),
        [flag, value] if flag == "--host" => HookHost::parse_arg(value).ok_or_else(|| {
            format!("invalid --host value {value:?}; expected one of: auto, claude, codex, gemini")
        }),
        [arg] if arg.starts_with("--host=") => {
            let value = arg.strip_prefix("--host=").unwrap_or("");
            HookHost::parse_arg(value).ok_or_else(|| {
                format!(
                    "invalid --host value {value:?}; expected one of: auto, claude, codex, gemini"
                )
            })
        }
        [flag] if flag == "--host" => {
            Err("missing value for --host; expected one of: auto, claude, codex, gemini".into())
        }
        _ => Err(format!(
            "unexpected arguments {args:?}; only optional `--host <auto|claude|codex|gemini>` is supported"
        )),
    }
}

/// Extracts every file path referenced by a Codex `apply_patch` document.
///
/// Recognizes `*** Update File:`, `*** Delete File:`, `*** Add File:`, and
/// `*** Move to:` headers, preserving first-seen order without duplicates.
pub fn extract_apply_patch_paths(patch: &str) -> Vec<String> {
    const PREFIXES: &[&str] = &[
        "*** Update File:",
        "*** Delete File:",
        "*** Add File:",
        "*** Move to:",
    ];
    let mut paths = Vec::new();
    for raw_line in patch.lines() {
        let line = raw_line.trim_end_matches('\r');
        for prefix in PREFIXES {
            if let Some(rest) = line.strip_prefix(prefix) {
                let candidate = rest.trim();
                if !candidate.is_empty() && !paths.iter().any(|existing| existing == candidate) {
                    paths.push(candidate.to_string());
                }
                break;
            }
        }
    }
    paths
}

fn shell_quote_arg(arg: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    if !arg.is_empty() && arg.chars().all(plain) {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

fn extract_shell_command(
    tool_input: Option<&Value>,
    allow_cmd_alias: bool,
) -> Result<String, Denial> {
    let Some(obj) = tool_input.and_then(Value::as_object) else {
        return Err(bad_input("shell tool call without a tool_input object"));
    };
    let field = obj.get("command").or_else(|| {
        if allow_cmd_alias {
            obj.get("cmd")
        } else {
            None
        }
    });
    let Some(value) = field else {
        return Err(bad_input(if allow_cmd_alias {
            "shell tool call without tool_input.command or tool_input.cmd"
        } else {
            "Bash call without tool_input.command"
        }));
    };
    match value {
        Value::String(cmd) => Ok(cmd.clone()),
        Value::Array(items) if allow_cmd_alias => {
            if items.is_empty() {
                return Err(bad_input("shell tool call with empty command array"));
            }
            let mut quoted = Vec::with_capacity(items.len());
            for item in items {
                let Some(part) = item.as_str() else {
                    return Err(bad_input(
                        "shell tool call command array contains non-string element",
                    ));
                };
                quoted.push(shell_quote_arg(part));
            }
            Ok(quoted.join(" "))
        }
        _ => Err(bad_input("shell tool call command has unsupported type")),
    }
}

fn extract_file_path(tool_input: Option<&Value>, allow_path_alias: bool) -> Result<String, Denial> {
    let Some(obj) = tool_input.and_then(Value::as_object) else {
        return Err(bad_input("file tool call without a tool_input object"));
    };
    let value = obj.get("file_path").and_then(Value::as_str).or_else(|| {
        if allow_path_alias {
            obj.get("path").and_then(Value::as_str)
        } else {
            None
        }
    });
    match value {
        Some(path) if !path.is_empty() => Ok(path.to_string()),
        _ => Err(bad_input(if allow_path_alias {
            "file tool call without tool_input.file_path or tool_input.path"
        } else {
            "file tool call without tool_input.file_path"
        })),
    }
}

fn extract_many_paths(tool_input: Option<&Value>) -> Result<Vec<String>, Denial> {
    let Some(obj) = tool_input.and_then(Value::as_object) else {
        return Err(bad_input(
            "read_many_files call without a tool_input object",
        ));
    };
    let Some(paths_val) = obj.get("paths") else {
        return Err(bad_input("read_many_files call without tool_input.paths"));
    };
    let Some(items) = paths_val.as_array() else {
        return Err(bad_input(
            "read_many_files tool_input.paths is not an array",
        ));
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let Some(path) = item.as_str() else {
            return Err(bad_input(
                "read_many_files tool_input.paths contains a non-string entry",
            ));
        };
        if !path.is_empty() {
            out.push(path.to_string());
        }
    }
    if let Some(include_val) = obj.get("include") {
        let Some(include_items) = include_val.as_array() else {
            return Err(bad_input(
                "read_many_files tool_input.include is not an array",
            ));
        };
        for item in include_items {
            let Some(path) = item.as_str() else {
                return Err(bad_input(
                    "read_many_files tool_input.include contains a non-string entry",
                ));
            };
            if !path.is_empty() {
                out.push(path.to_string());
            }
        }
    }
    Ok(out)
}

fn extract_apply_patch(tool_input: Option<&Value>) -> Result<Vec<String>, Denial> {
    let patch_text = match tool_input {
        Some(Value::String(raw)) => Some(raw.as_str()),
        Some(Value::Object(obj)) => obj
            .get("patch")
            .and_then(Value::as_str)
            .or_else(|| obj.get("input").and_then(Value::as_str)),
        _ => None,
    };
    let Some(patch) = patch_text.filter(|text| !text.trim().is_empty()) else {
        return Err(bad_input(
            "apply_patch call without tool_input.patch or tool_input.input",
        ));
    };
    let paths = extract_apply_patch_paths(patch);
    if paths.is_empty() {
        return Err(bad_input(
            "apply_patch call contains no recognized target file paths",
        ));
    }
    Ok(paths)
}

fn detect_host(payload: &Map<String, Value>, tool: &str) -> HookHost {
    let event = payload
        .get("hook_event_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    if event == "BeforeTool"
        || matches!(
            tool,
            "run_shell_command" | "write_file" | "replace" | "read_file" | "read_many_files"
        )
    {
        return HookHost::Gemini;
    }
    if matches!(
        tool,
        "shell" | "shell_command" | "exec_command" | "apply_patch"
    ) {
        return HookHost::Codex;
    }
    HookHost::Claude
}

fn extract_cwd(payload: &Map<String, Value>) -> Option<String> {
    payload
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.is_empty())
        .or_else(|| {
            payload
                .get("workspace_root")
                .and_then(Value::as_str)
                .filter(|cwd| !cwd.is_empty())
        })
        .map(str::to_string)
}

fn parse_action(
    host: HookHost,
    tool: &str,
    tool_input: Option<&Value>,
) -> Result<ToolAction, Denial> {
    match host {
        HookHost::Claude | HookHost::Auto => match tool {
            "Bash" => extract_shell_command(tool_input, false).map(ToolAction::Shell),
            "Read" => extract_file_path(tool_input, false).map(ToolAction::FileRead),
            "Write" | "Edit" | "MultiEdit" => {
                extract_file_path(tool_input, false).map(|path| ToolAction::FileWrite(vec![path]))
            }
            _ => Ok(ToolAction::Unknown),
        },
        HookHost::Codex => match tool {
            "Bash" | "shell" | "shell_command" | "exec_command" => {
                extract_shell_command(tool_input, true).map(ToolAction::Shell)
            }
            "apply_patch" => extract_apply_patch(tool_input).map(ToolAction::FileWrite),
            "write_file" | "edit_file" | "create_file" | "delete_file" | "Write" | "Edit"
            | "MultiEdit" => {
                extract_file_path(tool_input, true).map(|path| ToolAction::FileWrite(vec![path]))
            }
            "read_file" | "view_file" | "Read" => {
                extract_file_path(tool_input, true).map(ToolAction::FileRead)
            }
            _ => Ok(ToolAction::Unknown),
        },
        HookHost::Gemini => match tool {
            "run_shell_command" | "shell" => {
                extract_shell_command(tool_input, true).map(ToolAction::Shell)
            }
            "write_file" | "replace" | "edit_file" => {
                extract_file_path(tool_input, true).map(|path| ToolAction::FileWrite(vec![path]))
            }
            "read_file" => extract_file_path(tool_input, true).map(ToolAction::FileRead),
            "read_many_files" => extract_many_paths(tool_input).map(ToolAction::FileReadMany),
            _ => Ok(ToolAction::Unknown),
        },
    }
}

fn parse_payload_with_host(
    input: &[u8],
    requested: HookHost,
) -> (HookHost, Result<ParsedCall, Denial>) {
    let fallback_host = if requested == HookHost::Auto {
        HookHost::Claude
    } else {
        requested
    };
    let value: Value = match serde_json::from_slice(input) {
        Ok(value) => value,
        Err(_) => return (fallback_host, Err(bad_input("stdin is not valid JSON"))),
    };
    let Some(payload) = value.as_object() else {
        return (
            fallback_host,
            Err(bad_input("the payload is not a JSON object")),
        );
    };
    let tool = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let resolved_host = if requested == HookHost::Auto {
        detect_host(payload, tool)
    } else {
        requested
    };
    let cwd = extract_cwd(payload);
    let action = match parse_action(resolved_host, tool, payload.get("tool_input")) {
        Ok(action) => action,
        Err(denial) => return (resolved_host, Err(denial)),
    };
    (
        resolved_host,
        Ok(ParsedCall {
            resolved_host,
            cwd,
            action,
        }),
    )
}

/// Parses and normalizes a raw hook payload for `requested` host (auto-detecting
/// when `requested == HookHost::Auto`). Fails closed on invalid JSON,
/// non-object payloads, or missing required fields for guarded tools.
pub fn parse_payload(input: &[u8], requested: HookHost) -> Result<ParsedCall, Denial> {
    parse_payload_with_host(input, requested).1
}

fn check_read_path(raw: &str, ctx: &Ctx) -> Option<Denial> {
    let path = ctx.expand(raw);
    if let Some(denial) = crate::data_guard::check_file(raw, &path, ctx)
        .or_else(|| crate::secret_guard::check_read(&path))
    {
        return Some(denial);
    }
    // Check glob targets (e.g. Gemini `read_many_files` with `.env*`) against
    // existing secret files in the target directory.
    if crate::paths::has_glob(raw)
        && let Some(parent) = path.parent()
        && let Some(last) = path.file_name().and_then(|name| name.to_str())
        && let Ok(entries) = fs::read_dir(parent)
    {
        for entry in entries.flatten() {
            if let Ok(name) = entry.file_name().into_string()
                && crate::paths::glob_match(last, &name)
                && let Some(denial) = crate::secret_guard::check_read(&parent.join(&name))
            {
                return Some(denial);
            }
        }
    }
    None
}

fn check_write_path(raw: &str, ctx: &Ctx) -> Option<Denial> {
    let path = ctx.expand(raw);
    crate::data_guard::check_file(raw, &path, ctx)
        .or_else(|| crate::delete_guard::check_file(&path, ctx))
        .or_else(|| crate::git_files::check_file(&path))
}

/// Evaluates one normalized [`ToolAction`] against the full `aaf-hook` guard suite.
pub fn evaluate(action: ToolAction, ctx: Ctx) -> Option<Denial> {
    match action {
        ToolAction::Shell(command) => crate::bash(&command, ctx),
        ToolAction::FileRead(raw) => check_read_path(&raw, &ctx),
        ToolAction::FileReadMany(paths) => paths.iter().find_map(|raw| check_read_path(raw, &ctx)),
        ToolAction::FileWrite(paths) => paths.iter().find_map(|raw| check_write_path(raw, &ctx)),
        ToolAction::Unknown => None,
    }
}

/// Judges one hook payload for `host` (auto-detecting when `HookHost::Auto`)
/// and returns both the [`Decision`] and the resolved [`HookHost`].
pub fn decide_for_host(input: &[u8], env: &HookEnv, host: HookHost) -> (Decision, HookHost) {
    let (resolved_host, parsed) = parse_payload_with_host(input, host);
    let parsed = match parsed {
        Ok(parsed) => parsed,
        Err(denial) => return (Decision::Deny(denial), resolved_host),
    };
    let cwd = match (parsed.cwd.as_deref(), &env.cwd) {
        (Some(given), Some(base)) => base.join(given),
        (Some(given), None) => PathBuf::from(given),
        (None, Some(base)) => base.clone(),
        (None, None) => PathBuf::new(),
    };
    let ctx = Ctx::new(env, cwd);
    let decision = evaluate(parsed.action, ctx).map_or(Decision::Allow, Decision::Deny);
    (decision, parsed.resolved_host)
}

/// Renders a [`Decision`] into the JSON schema expected by `host`.
pub fn render_for_host(decision: &Decision, host: HookHost) -> Outcome {
    match decision {
        Decision::Allow => Outcome {
            stdout: "{}".to_string(),
            stderr: String::new(),
            code: 0,
        },
        Decision::Deny(denial) => {
            let message = denial.message();
            let body = match host {
                HookHost::Gemini => json!({
                    "decision": "deny",
                    "reason": message,
                    "hookSpecificOutput": {
                        "hookEventName": "BeforeTool",
                        "permissionDecision": "deny",
                        "permissionDecisionReason": message,
                    }
                }),
                HookHost::Claude | HookHost::Codex | HookHost::Auto => json!({
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "deny",
                        "permissionDecisionReason": message,
                    }
                }),
            };
            Outcome {
                stdout: body.to_string(),
                stderr: message,
                code: 2,
            }
        }
    }
}

/// `decide_for_host` + `render_for_host`, panic-safe so the hook never exits
/// with anything other than `0` (allow) or `2` (deny).
pub fn run_for_host(input: &[u8], env: &HookEnv, host: HookHost) -> Outcome {
    let fallback_host = if host == HookHost::Auto {
        HookHost::Claude
    } else {
        host
    };
    panic::catch_unwind(AssertUnwindSafe(|| {
        let (decision, resolved) = decide_for_host(input, env, host);
        render_for_host(&decision, resolved)
    }))
    .unwrap_or_else(|_| {
        render_for_host(
            &Decision::Deny(Denial::new(
                "hook_internal",
                "the guard crashed while judging this call",
                "report the command to the AutoAgentFactory maintainers",
            )),
            fallback_host,
        )
    })
}
