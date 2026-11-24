//! Host specifications, tool lists, and target enums for multi-host hook
//! registration (`claude`, `codex`, `gemini`, `antigravity`, `opencode`).

use super::SETTINGS_REL;

/// Repo-relative path of the Codex CLI hooks configuration file.
pub const CODEX_HOOKS_REL: &str = ".codex/hooks.json";

/// Repo-relative path of the Gemini CLI / Antigravity settings file.
pub const GEMINI_SETTINGS_REL: &str = ".gemini/settings.json";

/// Repo-relative path of the OpenCode hooks configuration file.
pub const OPENCODE_HOOKS_REL: &str = ".opencode/hooks.json";

/// Codex CLI tools that must be covered by the `PreToolUse` matcher.
pub const CODEX_TOOLS: [&str; 11] = [
    "Bash",
    "shell",
    "shell_command",
    "exec_command",
    "apply_patch",
    "write_file",
    "edit_file",
    "create_file",
    "delete_file",
    "read_file",
    "view_file",
];

/// Gemini CLI tools that must be covered by the `BeforeTool` matcher.
pub const GEMINI_TOOLS: [&str; 5] = [
    "run_shell_command",
    "write_file",
    "replace",
    "read_file",
    "read_many_files",
];

/// Antigravity tools that must be covered by the `BeforeTool` matcher.
pub const ANTIGRAVITY_TOOLS: [&str; 11] = [
    "run_command",
    "run_shell_command",
    "write_to_file",
    "replace_file_content",
    "multi_replace_file_content",
    "write_file",
    "edit_file",
    "replace",
    "view_file",
    "read_file",
    "read_many_files",
];

/// OpenCode tools that must be covered by the `PreToolUse` matcher.
pub const OPENCODE_TOOLS: [&str; 6] = ["bash", "shell", "edit", "write", "patch", "read"];

/// Minimum allowed `timeout` for Codex / OpenCode hooks (in seconds).
const MIN_CODEX_TIMEOUT_SECS: u64 = 30;

/// Minimum allowed `timeout` for Gemini / Antigravity hooks (in milliseconds).
const MIN_GEMINI_TIMEOUT_MS: u64 = 30_000;

/// Supported coding-agent host targets for hook registration and diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookHostTarget {
    Claude,
    Codex,
    Gemini,
    Antigravity,
    Opencode,
}

impl HookHostTarget {
    pub fn as_str(self) -> &'static str {
        match self {
            HookHostTarget::Claude => "claude",
            HookHostTarget::Codex => "codex",
            HookHostTarget::Gemini => "gemini",
            HookHostTarget::Antigravity => "antigravity",
            HookHostTarget::Opencode => "opencode",
        }
    }

    pub fn settings_rel(self) -> &'static str {
        match self {
            HookHostTarget::Claude => SETTINGS_REL,
            HookHostTarget::Codex => CODEX_HOOKS_REL,
            HookHostTarget::Gemini | HookHostTarget::Antigravity => GEMINI_SETTINGS_REL,
            HookHostTarget::Opencode => OPENCODE_HOOKS_REL,
        }
    }
}

/// Diagnostic status of a single host's hook configuration (for `aaf doctor`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostHookStatus {
    Registered(super::Registered),
    NotConfigured,
    Invalid(String),
}

/// Parses a `--host` / `--hook-host` argument value (`claude`, `codex`, `gemini`,
/// `antigravity`, `opencode`, `all`, or a comma-separated list of host names)
/// into deduplicated [`HookHostTarget`]s.
pub fn parse_host_arg(raw: &str) -> Result<Vec<HookHostTarget>, String> {
    const VALID: &str = "claude, codex, gemini, antigravity, opencode, all";
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!("empty host value; expected one of: {VALID}"));
    }
    let mut out = Vec::new();
    for part in trimmed.split(',') {
        let token = part.trim().to_ascii_lowercase();
        let targets: &[HookHostTarget] = match token.as_str() {
            "claude" => &[HookHostTarget::Claude],
            "codex" => &[HookHostTarget::Codex],
            "gemini" => &[HookHostTarget::Gemini],
            "antigravity" => &[HookHostTarget::Antigravity],
            "opencode" => &[HookHostTarget::Opencode],
            "all" => &[
                HookHostTarget::Claude,
                HookHostTarget::Codex,
                HookHostTarget::Gemini,
                HookHostTarget::Opencode,
            ],
            _ => {
                return Err(format!(
                    "unknown hook host {raw:?}; expected one of: {VALID}"
                ));
            }
        };
        for target in targets {
            if !out.contains(target) {
                out.push(*target);
            }
        }
    }
    Ok(out)
}

/// Canonical `PreToolUse` matcher string for Codex CLI.
pub fn codex_matcher() -> String {
    CODEX_TOOLS.join("|")
}

/// Canonical `BeforeTool` matcher string for Gemini CLI.
pub fn gemini_matcher() -> String {
    GEMINI_TOOLS.join("|")
}

/// Canonical `BeforeTool` matcher string for Antigravity.
pub fn antigravity_matcher() -> String {
    ANTIGRAVITY_TOOLS.join("|")
}

/// Canonical `PreToolUse` matcher string for OpenCode.
pub fn opencode_matcher() -> String {
    OPENCODE_TOOLS.join("|")
}

#[derive(Clone, Copy)]
pub(super) struct HostSpec {
    pub settings_rel: &'static str,
    pub event_key: &'static str,
    pub host_flag: &'static str,
    pub tools: &'static [&'static str],
    pub min_timeout: u64,
    pub timeout_unit: &'static str,
    pub check_gemini_hooks_config: bool,
}

pub(super) const CODEX_SPEC: HostSpec = HostSpec {
    settings_rel: CODEX_HOOKS_REL,
    event_key: "PreToolUse",
    host_flag: "codex",
    tools: &CODEX_TOOLS,
    min_timeout: MIN_CODEX_TIMEOUT_SECS,
    timeout_unit: "seconds",
    check_gemini_hooks_config: false,
};

pub(super) const GEMINI_SPEC: HostSpec = HostSpec {
    settings_rel: GEMINI_SETTINGS_REL,
    event_key: "BeforeTool",
    host_flag: "gemini",
    tools: &GEMINI_TOOLS,
    min_timeout: MIN_GEMINI_TIMEOUT_MS,
    timeout_unit: "milliseconds",
    check_gemini_hooks_config: true,
};

pub(super) const ANTIGRAVITY_SPEC: HostSpec = HostSpec {
    settings_rel: GEMINI_SETTINGS_REL,
    event_key: "BeforeTool",
    host_flag: "antigravity",
    tools: &ANTIGRAVITY_TOOLS,
    min_timeout: MIN_GEMINI_TIMEOUT_MS,
    timeout_unit: "milliseconds",
    check_gemini_hooks_config: true,
};

pub(super) const OPENCODE_SPEC: HostSpec = HostSpec {
    settings_rel: OPENCODE_HOOKS_REL,
    event_key: "PreToolUse",
    host_flag: "opencode",
    tools: &OPENCODE_TOOLS,
    min_timeout: MIN_CODEX_TIMEOUT_SECS,
    timeout_unit: "seconds",
    check_gemini_hooks_config: false,
};
