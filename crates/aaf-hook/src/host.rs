//! Multi-host hook payload adapters (Claude Code, Codex CLI, Gemini CLI,
//! Antigravity, OpenCode).
//!
//! Normalizes tool-call payloads from all supported coding-agent hosts
//! into a single [`ToolAction`] so every host runs through the exact same
//! guard suite, and renders the host-specific allow/deny JSON response.

mod extra;

use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;

use serde_json::{Map, Value, json};

pub use extra::extract_apply_patch_paths;
use extra::{
    extract_apply_patch, extract_cwd, extract_cwd_override, extract_file_path, extract_many_paths,
    extract_shell_command, parse_antigravity_action, parse_opencode_action,
};

use crate::{Ctx, Decision, Denial, HookEnv, Outcome};

const INPUT_RULE: &str = "hook_input";
const INPUT_FIX: &str =
    "send a valid PreToolUse, BeforeTool, or tool.execute.before JSON payload on stdin";
const VALID_HOSTS: &str = "auto, claude, codex, gemini, antigravity, opencode";

/// Coding-agent host invoking `aaf-hook`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookHost {
    Auto,
    Claude,
    Codex,
    Gemini,
    Antigravity,
    Opencode,
}

impl HookHost {
    /// Parses a `--host` flag value (`auto`, `claude`, `codex`, `gemini`, `antigravity`, `opencode`).
    pub fn parse_arg(value: &str) -> Option<HookHost> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(HookHost::Auto),
            "claude" => Some(HookHost::Claude),
            "codex" => Some(HookHost::Codex),
            "gemini" => Some(HookHost::Gemini),
            "antigravity" => Some(HookHost::Antigravity),
            "opencode" => Some(HookHost::Opencode),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            HookHost::Auto => "auto",
            HookHost::Claude => "claude",
            HookHost::Codex => "codex",
            HookHost::Gemini => "gemini",
            HookHost::Antigravity => "antigravity",
            HookHost::Opencode => "opencode",
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
    Mcp { tool: String, input: Option<Value> },
    Unknown,
}

/// Parsed hook payload with its resolved host, working directory, and action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCall {
    pub resolved_host: HookHost,
    pub cwd: Option<String>,
    pub cwd_override: Option<String>,
    pub action: ToolAction,
}

fn bad_input(reason: impl Into<String>) -> Denial {
    Denial::new(INPUT_RULE, reason, INPUT_FIX)
}

/// Parses CLI arguments after `argv[0]` (e.g., `env::args().skip(1)`).
///
/// Accepts no arguments (defaults to [`HookHost::Auto`]),
/// `--host <auto|claude|codex|gemini|antigravity|opencode>`, or `--host=<value>`.
/// Fails closed on any unknown flag, missing value, or extra argument.
pub fn parse_cli_host(args: &[String]) -> Result<HookHost, String> {
    match args {
        [] => Ok(HookHost::Auto),
        [flag, value] if flag == "--host" => HookHost::parse_arg(value).ok_or_else(|| {
            format!("invalid --host value {value:?}; expected one of: {VALID_HOSTS}")
        }),
        [arg] if arg.starts_with("--host=") => {
            let value = arg.strip_prefix("--host=").unwrap_or("");
            HookHost::parse_arg(value).ok_or_else(|| {
                format!("invalid --host value {value:?}; expected one of: {VALID_HOSTS}")
            })
        }
        [flag] if flag == "--host" => Err(format!(
            "missing value for --host; expected one of: {VALID_HOSTS}"
        )),
        _ => Err(format!(
            "unexpected arguments {args:?}; only optional `--host <auto|claude|codex|gemini|antigravity|opencode>` is supported"
        )),
    }
}

fn detect_host(payload: &Map<String, Value>, tool: &str) -> HookHost {
    let event = payload
        .get("hook_event_name")
        .or_else(|| payload.get("event"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let is_antigravity_tool = matches!(
        tool,
        "run_command" | "write_to_file" | "replace_file_content" | "multi_replace_file_content"
    );
    if event == "tool.execute.before"
        || matches!(tool, "bash" | "edit" | "write" | "patch" | "read")
        || (!payload.contains_key("tool_name")
            && payload.contains_key("tool")
            && payload.contains_key("args")
            && !is_antigravity_tool)
    {
        return HookHost::Opencode;
    }
    if is_antigravity_tool {
        return HookHost::Antigravity;
    }
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
        "shell"
            | "shell_command"
            | "exec_command"
            | "apply_patch"
            | "delete_file"
            | "edit_file"
            | "create_file"
            | "view_file"
    ) {
        return HookHost::Codex;
    }
    HookHost::Claude
}

fn parse_action(
    host: HookHost,
    tool: &str,
    tool_input: Option<&Value>,
) -> Result<ToolAction, Denial> {
    if crate::gh_bot_guard::is_github_mcp_tool(tool) {
        return Ok(ToolAction::Mcp {
            tool: tool.to_string(),
            input: tool_input.cloned(),
        });
    }
    match host {
        HookHost::Claude | HookHost::Auto => match tool {
            "Bash" => extract_shell_command(tool_input, true, false).map(ToolAction::Shell),
            "Read" => extract_file_path(tool_input, false).map(ToolAction::FileRead),
            "Write" | "Edit" | "MultiEdit" => {
                extract_file_path(tool_input, false).map(|path| ToolAction::FileWrite(vec![path]))
            }
            _ => Ok(ToolAction::Unknown),
        },
        HookHost::Codex => match tool {
            "Bash" | "shell" | "shell_command" | "exec_command" => {
                extract_shell_command(tool_input, true, true).map(ToolAction::Shell)
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
            "run_shell_command" | "run_command" | "shell" => {
                extract_shell_command(tool_input, true, true).map(ToolAction::Shell)
            }
            "write_file"
            | "write_to_file"
            | "replace"
            | "replace_file_content"
            | "multi_replace_file_content"
            | "edit_file" => {
                extract_file_path(tool_input, true).map(|path| ToolAction::FileWrite(vec![path]))
            }
            "read_file" | "view_file" => {
                extract_file_path(tool_input, true).map(ToolAction::FileRead)
            }
            "read_many_files" => extract_many_paths(tool_input).map(ToolAction::FileReadMany),
            _ => Ok(ToolAction::Unknown),
        },
        HookHost::Antigravity => parse_antigravity_action(tool, tool_input),
        HookHost::Opencode => parse_opencode_action(tool, tool_input),
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
        .or_else(|| payload.get("tool"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let tool_input = payload
        .get("tool_input")
        .or_else(|| payload.get("args"))
        .or_else(|| payload.get("arguments"));
    let resolved_host = if requested == HookHost::Auto {
        detect_host(payload, tool)
    } else {
        requested
    };
    let cwd = extract_cwd(payload);
    let cwd_override = match extract_cwd_override(tool_input) {
        Ok(dir) => dir,
        Err(denial) => return (resolved_host, Err(denial)),
    };
    let action = match parse_action(resolved_host, tool, tool_input) {
        Ok(action) => action,
        Err(denial) => return (resolved_host, Err(denial)),
    };
    (
        resolved_host,
        Ok(ParsedCall {
            resolved_host,
            cwd,
            cwd_override,
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
    let check_one = |path: &std::path::Path| {
        crate::data_guard::check_file(raw, path, ctx)
            .or_else(|| crate::secret_guard::check_read_target(raw, path, ctx))
    };
    let path = ctx.expand(raw);
    check_one(&path).or_else(|| {
        if !raw.contains("..") {
            return None;
        }
        let real_dd = ctx.expand_real_dotdot(raw);
        (real_dd != path).then(|| check_one(&real_dd)).flatten()
    })
}

fn check_write_path(raw: &str, ctx: &Ctx) -> Option<Denial> {
    let check_one = |path: &std::path::Path| {
        crate::data_guard::check_file(raw, path, ctx)
            .or_else(|| crate::delete_guard::check_file(path, ctx))
            .or_else(|| crate::git_files::check_file(path))
            .or_else(|| crate::main_branch_guard::check_file(path, ctx))
    };
    let path = ctx.expand(raw);
    check_one(&path).or_else(|| {
        if !raw.contains("..") {
            return None;
        }
        let real_dd = ctx.expand_real_dotdot(raw);
        (real_dd != path).then(|| check_one(&real_dd)).flatten()
    })
}

/// Evaluates one normalized [`ToolAction`] against the full `aaf-hook` guard suite.
pub fn evaluate(action: ToolAction, ctx: Ctx) -> Option<Denial> {
    match action {
        ToolAction::Shell(command) => crate::bash(&command, ctx),
        ToolAction::FileRead(raw) => check_read_path(&raw, &ctx),
        ToolAction::FileReadMany(paths) => paths.iter().find_map(|raw| check_read_path(raw, &ctx)),
        ToolAction::FileWrite(paths) => paths.iter().find_map(|raw| check_write_path(raw, &ctx)),
        ToolAction::Mcp { tool, input } => crate::gh_bot_guard::check_mcp(&tool, input.as_ref()),
        ToolAction::Unknown => None,
    }
}

/// Judges one hook payload for `host` (auto-detecting when `HookHost::Auto`)
/// and returns both the [`Decision`] and the resolved [`HookHost`].
pub fn decide_for_host(input: &[u8], env: &HookEnv, host: HookHost) -> (Decision, HookHost) {
    let _resolving = crate::paths::Resolving::start();
    let (resolved_host, parsed) = parse_payload_with_host(input, host);
    let parsed = match parsed {
        Ok(parsed) => parsed,
        Err(denial) => return (Decision::Deny(denial), resolved_host),
    };
    let base_cwd = match (parsed.cwd.as_deref(), &env.cwd) {
        (Some(given), Some(base)) => base.join(given),
        (Some(given), None) => PathBuf::from(given),
        (None, Some(base)) => base.clone(),
        (None, None) => PathBuf::new(),
    };
    let base_ctx = Ctx::new(env, base_cwd);
    let ctx = match parsed.cwd_override.as_deref() {
        Some(override_dir) => {
            let moved =
                crate::paths::expand_real_dotdot(override_dir, &base_ctx.cwd, env.home.as_deref());
            base_ctx.with_cwd(moved)
        }
        None => base_ctx,
    };
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
                HookHost::Gemini | HookHost::Antigravity => json!({
                    "decision": "deny",
                    "reason": message,
                    "hookSpecificOutput": {
                        "hookEventName": "BeforeTool",
                        "permissionDecision": "deny",
                        "permissionDecisionReason": message,
                    }
                }),
                HookHost::Opencode => json!({
                    "decision": "deny",
                    "reason": message,
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
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
