//! Unified registry of 10 coding-agent CLIs with fallback routing and PATH inventory.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::find_on_path;

/// Hook enforcement capability provided by a coding-agent CLI host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HookSupport {
    ClaudePreToolUse,
    CodexPreToolUse,
    GeminiBeforeTool,
    SandboxOnly,
}

impl HookSupport {
    /// Short machine-readable label for tables and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            HookSupport::ClaudePreToolUse => "claude_pre_tool_use",
            HookSupport::CodexPreToolUse => "codex_pre_tool_use",
            HookSupport::GeminiBeforeTool => "gemini_before_tool",
            HookSupport::SandboxOnly => "sandbox_only",
        }
    }
}

impl fmt::Display for HookSupport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Static descriptor for a supported coding-agent CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CliDescriptor {
    pub id: &'static str,
    pub display_name: &'static str,
    pub binaries: &'static [&'static str],
    pub hook_support: HookSupport,
    pub supports_model_flag: bool,
    pub supports_effort_flag: bool,
    pub supports_read_only_flag: bool,
    pub headless_env: &'static [(&'static str, &'static str)],
}

/// PATH discovery record for one built-in coding-agent CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledExecutor {
    pub id: String,
    pub display_name: String,
    pub installed: bool,
    pub resolved_binary: Option<PathBuf>,
    pub hook_support: HookSupport,
    pub headless_env_count: usize,
}

/// Result of resolving a primary executor with ordered fallback routing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedExecutor {
    pub descriptor: &'static CliDescriptor,
    pub binary_path: PathBuf,
    pub used_fallback: bool,
    pub skipped: Vec<(String, String)>,
}

const GEMINI_HEADLESS_ENV: &[(&str, &str)] = &[("GEMINI_CLI_TRUST_WORKSPACE", "true")];

const BUILTIN_EXECUTORS: [CliDescriptor; 10] = [
    CliDescriptor {
        id: "claude",
        display_name: "Claude Code",
        binaries: &["claude"],
        hook_support: HookSupport::ClaudePreToolUse,
        supports_model_flag: true,
        supports_effort_flag: true,
        supports_read_only_flag: true,
        headless_env: &[],
    },
    CliDescriptor {
        id: "codex",
        display_name: "OpenAI Codex CLI",
        binaries: &["codex"],
        hook_support: HookSupport::CodexPreToolUse,
        supports_model_flag: true,
        supports_effort_flag: false,
        supports_read_only_flag: true,
        headless_env: &[],
    },
    CliDescriptor {
        id: "gemini",
        display_name: "Google Gemini CLI",
        binaries: &["gemini"],
        hook_support: HookSupport::GeminiBeforeTool,
        supports_model_flag: true,
        supports_effort_flag: false,
        supports_read_only_flag: true,
        headless_env: GEMINI_HEADLESS_ENV,
    },
    CliDescriptor {
        id: "opencode",
        display_name: "OpenCode",
        binaries: &["opencode"],
        hook_support: HookSupport::SandboxOnly,
        supports_model_flag: true,
        supports_effort_flag: false,
        supports_read_only_flag: false,
        headless_env: &[],
    },
    CliDescriptor {
        id: "cursor",
        display_name: "Cursor CLI",
        binaries: &["cursor-agent", "agent"],
        hook_support: HookSupport::SandboxOnly,
        supports_model_flag: true,
        supports_effort_flag: false,
        supports_read_only_flag: false,
        headless_env: &[],
    },
    CliDescriptor {
        id: "antigravity",
        display_name: "Antigravity CLI",
        binaries: &["agy", "antigravity"],
        hook_support: HookSupport::SandboxOnly,
        supports_model_flag: true,
        supports_effort_flag: true,
        supports_read_only_flag: true,
        headless_env: &[],
    },
    CliDescriptor {
        id: "codebuddy",
        display_name: "CodeBuddy CLI",
        binaries: &["codebuddy"],
        hook_support: HookSupport::SandboxOnly,
        supports_model_flag: true,
        supports_effort_flag: false,
        supports_read_only_flag: false,
        headless_env: &[],
    },
    CliDescriptor {
        id: "kimi",
        display_name: "Kimi Code",
        binaries: &["kimi"],
        hook_support: HookSupport::SandboxOnly,
        supports_model_flag: true,
        supports_effort_flag: false,
        supports_read_only_flag: false,
        headless_env: &[],
    },
    CliDescriptor {
        id: "pi",
        display_name: "Pi Agent CLI",
        binaries: &["pi"],
        hook_support: HookSupport::SandboxOnly,
        supports_model_flag: true,
        supports_effort_flag: true,
        supports_read_only_flag: false,
        headless_env: &[],
    },
    CliDescriptor {
        id: "deepseek",
        display_name: "DeepSeek Harness",
        binaries: &["deepseek-harness", "dsh"],
        hook_support: HookSupport::SandboxOnly,
        supports_model_flag: true,
        supports_effort_flag: false,
        supports_read_only_flag: false,
        headless_env: &[],
    },
];

/// Returns all 10 built-in coding-agent CLI descriptors in canonical order.
pub fn builtin_executors() -> &'static [CliDescriptor] {
    &BUILTIN_EXECUTORS
}

/// Looks up a built-in CLI descriptor by canonical id or common case-insensitive alias.
pub fn find_descriptor(id: &str) -> Option<&'static CliDescriptor> {
    let normalized = id.trim().to_ascii_lowercase();
    let canonical = match normalized.as_str() {
        "claude" | "claude-code" | "claudecode" => "claude",
        "codex" | "codex-cli" | "openai-codex" => "codex",
        "gemini" | "gemini-cli" => "gemini",
        "opencode" | "open-code" => "opencode",
        "cursor" | "cursor-cli" | "cursor-agent" | "agent" => "cursor",
        "antigravity" | "antigravity-cli" | "agy" => "antigravity",
        "codebuddy" | "code-buddy" | "codebuddy-cli" => "codebuddy",
        "kimi" | "kimi-code" | "kimi-cli" => "kimi",
        "pi" | "pi-agent" | "pi-cli" => "pi",
        "deepseek" | "deepseek-harness" | "deepseek-cli" | "dsh" => "deepseek",
        _ => return None,
    };
    BUILTIN_EXECUTORS.iter().find(|desc| desc.id == canonical)
}

/// Alias for [`find_descriptor`]: looks up a built-in CLI descriptor by id or alias.
pub fn find_cli(id: &str) -> Option<&'static CliDescriptor> {
    find_descriptor(id)
}

/// Validates a CLI flag argument value (such as `model` or `effort`) so it is
/// a single safe token that cannot inject extra flags or control characters.
pub fn validate_flag_value(key: &str, value: &str) -> Result<(), String> {
    if value.starts_with('-') || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!(
            "invalid {key} value {value:?}: must be a single token without leading '-' or whitespace"
        ));
    }
    Ok(())
}

/// Builds a deterministic, shell-free headless argv for one coding-agent CLI
/// assuming OS sandbox confinement (`sandboxed = true`).
///
/// Rejects empty binaries/prompts and rejects `model` or `effort` values
/// containing whitespace, control characters, or a leading `-`.
pub fn build_headless_argv(
    desc: &CliDescriptor,
    binary: &str,
    prompt: &str,
    model: Option<&str>,
    effort: Option<&str>,
    read_only: bool,
) -> Result<Vec<String>, String> {
    build_headless_argv_confined(desc, binary, prompt, model, effort, read_only, true)
}

/// Like [`build_headless_argv`], with explicit control over whether OS sandbox
/// confinement is active. Non-interactive bypass flags (`bypassPermissions`,
/// `--full-auto`, `--approval-mode yolo`, `--dangerously-skip-permissions`) are
/// emitted only when `!read_only && sandboxed`.
pub fn build_headless_argv_confined(
    desc: &CliDescriptor,
    binary: &str,
    prompt: &str,
    model: Option<&str>,
    effort: Option<&str>,
    read_only: bool,
    sandboxed: bool,
) -> Result<Vec<String>, String> {
    if binary.trim().is_empty() || binary.chars().any(char::is_control) {
        return Err("binary path must be non-empty and free of control characters".to_string());
    }
    if prompt.trim().is_empty() {
        return Err("prompt must not be empty".to_string());
    }
    if let Some(m) = model {
        validate_flag_value("model", m)?;
    }
    if let Some(e) = effort {
        validate_flag_value("effort", e)?;
    }
    let clean_model = model.filter(|m| !m.is_empty());
    let clean_effort = effort.filter(|e| !e.is_empty());

    let mut argv = vec![binary.to_string()];
    match desc.id {
        "claude" => {
            argv.extend(
                ["-p", "--output-format", "stream-json", "--verbose"]
                    .into_iter()
                    .map(String::from),
            );
            if read_only {
                argv.extend(["--permission-mode", "plan"].into_iter().map(String::from));
            } else if sandboxed {
                argv.extend(
                    ["--permission-mode", "bypassPermissions"]
                        .into_iter()
                        .map(String::from),
                );
            } else {
                argv.extend(
                    [
                        "--permission-mode",
                        "acceptEdits",
                        "--permission-prompts",
                        "none",
                    ]
                    .into_iter()
                    .map(String::from),
                );
            }
            if let Some(m) = clean_model {
                argv.extend(["--model".to_string(), m.to_string()]);
            }
            if let Some(e) = clean_effort {
                argv.extend(["--effort".to_string(), e.to_string()]);
            }
            argv.push("--".to_string());
            argv.push(prompt.to_string());
        }
        "codex" => {
            argv.push("exec".to_string());
            if read_only {
                argv.extend(["--sandbox", "read-only"].into_iter().map(String::from));
            } else if sandboxed {
                argv.push("--full-auto".to_string());
            }
            if let Some(m) = clean_model {
                argv.extend(["--model".to_string(), m.to_string()]);
            }
            argv.push("--".to_string());
            argv.push(prompt.to_string());
        }
        "gemini" => {
            if read_only {
                argv.extend(["--approval-mode", "plan"].into_iter().map(String::from));
            } else if sandboxed {
                argv.extend(["--approval-mode", "yolo"].into_iter().map(String::from));
            }
            if let Some(m) = clean_model {
                argv.extend(["--model".to_string(), m.to_string()]);
            }
            argv.push("-p".to_string());
            argv.push(prompt.to_string());
        }
        "opencode" => {
            argv.push("run".to_string());
            if let Some(m) = clean_model {
                argv.extend(["--model".to_string(), m.to_string()]);
            }
            argv.push("--".to_string());
            argv.push(prompt.to_string());
        }
        "kimi" => {
            argv.push("--print".to_string());
            if let Some(m) = clean_model {
                argv.extend(["--model".to_string(), m.to_string()]);
            }
            argv.push("-p".to_string());
            argv.push(prompt.to_string());
        }
        "pi" => {
            if let Some(m) = clean_model {
                argv.extend(["--model".to_string(), m.to_string()]);
            }
            if let Some(e) = clean_effort {
                argv.extend(["--thinking".to_string(), e.to_string()]);
            }
            argv.push("-p".to_string());
            argv.push(prompt.to_string());
        }
        "antigravity" => {
            argv.push("--disable-slash-commands".to_string());
            if read_only {
                argv.extend(["--mode".to_string(), "plan".to_string()]);
            } else if sandboxed {
                argv.extend(
                    ["--mode", "auto", "--dangerously-skip-permissions"]
                        .into_iter()
                        .map(String::from),
                );
            } else {
                argv.extend(["--mode".to_string(), "accept-edits".to_string()]);
            }
            if let Some(m) = clean_model {
                argv.extend(["--model".to_string(), m.to_string()]);
            }
            if let Some(e) = clean_effort {
                argv.extend(["--effort".to_string(), e.to_string()]);
            }
            argv.push(format!("-p={prompt}"));
        }
        "cursor" | "codebuddy" | "deepseek" => {
            if let Some(m) = clean_model {
                argv.extend(["--model".to_string(), m.to_string()]);
            }
            argv.push("-p".to_string());
            argv.push(prompt.to_string());
        }
        _ => {
            if desc.supports_model_flag
                && let Some(m) = clean_model
            {
                argv.extend(["--model".to_string(), m.to_string()]);
            }
            if desc.supports_effort_flag
                && let Some(e) = clean_effort
            {
                argv.extend(["--effort".to_string(), e.to_string()]);
            }
            argv.push("-p".to_string());
            argv.push(prompt.to_string());
        }
    }
    Ok(argv)
}

/// Resolves the first matching binary for `desc` on `path_var`.
pub fn resolve_binary(desc: &CliDescriptor, path_var: &OsStr) -> Option<PathBuf> {
    desc.binaries
        .iter()
        .find_map(|candidate| find_on_path(path_var, candidate))
}

/// Resolves a built-in CLI's `{prompt}` command template from `path_var`.
pub(super) fn resolve_generic_template(
    exec: &str,
    model: &str,
    effort: &str,
    path_var: Option<&OsStr>,
) -> aaf_domain::ports::PortResult<Vec<String>> {
    use aaf_domain::ports::PortError;
    let desc = BUILTIN_EXECUTORS
        .iter()
        .find(|d| d.id == exec)
        .ok_or_else(|| PortError::Invalid(format!("unknown executor {exec:?}")))?;
    let bin = path_var
        .and_then(|p| resolve_binary(desc, p))
        .ok_or_else(|| {
            PortError::NotFound(format!(
                "none of [{}] for executor {:?} found on PATH",
                desc.binaries.join(", "),
                desc.id
            ))
        })?;
    let clean_model = (!model.is_empty()).then_some(model);
    let clean_effort = (!effort.is_empty()).then_some(effort);
    build_headless_argv_confined(
        desc,
        &bin.to_string_lossy(),
        "{prompt}",
        clean_model,
        clean_effort,
        false,
        false,
    )
    .map_err(PortError::Invalid)
}

/// Discovers which of the 10 built-in coding-agent CLIs are installed on `path_var`
/// using purely filesystem lookups (no subprocesses spawned).
pub fn discover_inventory(path_var: &OsStr) -> Vec<InstalledExecutor> {
    BUILTIN_EXECUTORS
        .iter()
        .map(|desc| {
            let resolved_binary = resolve_binary(desc, path_var);
            InstalledExecutor {
                id: desc.id.to_string(),
                display_name: desc.display_name.to_string(),
                installed: resolved_binary.is_some(),
                resolved_binary,
                hook_support: desc.hook_support,
                headless_env_count: desc.headless_env.len(),
            }
        })
        .collect()
}

/// Resolves `primary` or the first available entry in `fallbacks` against `path_var`,
/// skipping any executor listed in `unavailable` (for example cooling down or
/// rate-limited) and recording every skipped candidate in an explicit audit trail.
pub fn resolve_with_fallback(
    primary: &str,
    fallbacks: &[String],
    path_var: &OsStr,
    unavailable: &[&str],
) -> Result<ResolvedExecutor, String> {
    let unavailable_ids: HashSet<String> = unavailable
        .iter()
        .map(|entry| {
            find_descriptor(entry)
                .map(|d| d.id.to_string())
                .unwrap_or_else(|| entry.trim().to_ascii_lowercase())
        })
        .collect();

    let mut skipped: Vec<(String, String)> = Vec::new();
    let mut tried_ids: HashSet<&'static str> = HashSet::new();

    for (idx, raw) in std::iter::once(primary)
        .chain(fallbacks.iter().map(String::as_str))
        .enumerate()
    {
        let Some(desc) = find_descriptor(raw) else {
            skipped.push((raw.to_string(), "unknown executor".to_string()));
            continue;
        };
        if !tried_ids.insert(desc.id) {
            skipped.push((desc.id.to_string(), "already evaluated".to_string()));
            continue;
        }
        if unavailable_ids.contains(desc.id) {
            skipped.push((
                desc.id.to_string(),
                "unavailable (cooling down or rate-limited)".to_string(),
            ));
            continue;
        }
        let Some(binary_path) = resolve_binary(desc, path_var) else {
            skipped.push((
                desc.id.to_string(),
                format!("not found on PATH (tried: {})", desc.binaries.join(", ")),
            ));
            continue;
        };
        return Ok(ResolvedExecutor {
            descriptor: desc,
            binary_path,
            used_fallback: idx > 0,
            skipped,
        });
    }

    let summary = skipped
        .iter()
        .map(|(name, reason)| format!("{name}: {reason}"))
        .collect::<Vec<_>>()
        .join("; ");
    Err(format!(
        "no available executor for primary={primary:?} ({summary})"
    ))
}
