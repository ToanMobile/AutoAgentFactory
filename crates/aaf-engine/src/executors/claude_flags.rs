//! Command-line flag assembly for [`super::claude::ClaudeCodeRunner`].
//!
//! Separated from `claude.rs` to keep executor files concise and easy to audit.

use aaf_domain::ports::{AgentRequest, InfraFailure, InfraKind, PortError, PortResult};

/// Wording Claude Code CLI emits when OAuth token refresh or macOS Keychain
/// persistence fails (e.g. inside Seatbelt sandbox when an access token expires).
/// Matched only on API-flagged errors or zero-token failed runs so agent prose
/// never triggers false positives.
const OAUTH_AUTH_WORDING: [&str; 5] = [
    "failed to refresh oauth token",
    "can't save to the keychain",
    "oauth token has expired",
    "invalid authentication credentials",
    "not logged in · please run /login",
];

pub(super) fn auth_wording_infra(text: &str, max_chars: usize) -> Option<InfraFailure> {
    let lower = text.to_lowercase();
    OAUTH_AUTH_WORDING
        .iter()
        .any(|words| lower.contains(words))
        .then(|| InfraFailure {
            kind: InfraKind::Auth,
            raw: text.chars().take(max_chars).collect(),
        })
}

/// Write, shell, subagent, web, and notebook tools disabled on `read_only`
/// Claude runs.
///
/// Read-only in-process inspection tools (`Read`, `Grep`, `Glob`, and
/// `StructuredOutput`) remain enabled so auditors and planners keep full
/// codebase-verification capability, while `Bash` (auto-denied in plan mode)
/// and `Agent` (which would bypass parent `--max-turns` via unconstrained
/// subagents) are blocked cleanly.
pub(crate) const READ_ONLY_DISALLOWED_TOOLS: &str =
    "Edit,Write,NotebookEdit,WebFetch,WebSearch,Bash,Agent";

fn flag_value(key: &str, value: &str) -> PortResult<()> {
    if value.starts_with('-') || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(PortError::Invalid(format!(
            "run.{key} = {value:?} is not a single word"
        )));
    }
    Ok(())
}

/// Builds the full `claude` argv for `request` (before sandboxing).
///
/// Permission & MCP confinement (`claude --help`, 2.1.332):
/// - Read-only (`sandbox.read_only == true`): `--permission-mode plan`,
///   `--no-session-persistence`, `--disable-slash-commands`,
///   `--disallowedTools <write/shell/web tools>`, and `--strict-mcp-config`
///   (fail-closed: zero external MCP schemas or skill catalogs loaded).
/// - Sandboxed write (or explicit non-persistent opt-in): `bypassPermissions`.
/// - Default unsandboxed write: `acceptEdits` + `--permission-prompts none`.
pub(super) fn build_claude_args(
    request: &AgentRequest,
    model: &str,
    effort: &str,
    allow_unsandboxed_bypass: bool,
) -> PortResult<Vec<String>> {
    if request.prompt.is_empty() {
        return Err(PortError::Invalid("empty prompt".to_string()));
    }
    let mut args: Vec<String> = ["-p", "--output-format", "stream-json", "--verbose"]
        .into_iter()
        .map(String::from)
        .collect();
    if request.sandbox.read_only {
        args.extend(
            [
                "--permission-mode",
                "plan",
                "--no-session-persistence",
                "--disable-slash-commands",
                "--disallowedTools",
                READ_ONLY_DISALLOWED_TOOLS,
                "--strict-mcp-config",
            ]
            .map(String::from),
        );
    } else if request.sandbox.enabled || allow_unsandboxed_bypass {
        args.extend(["--permission-mode", "bypassPermissions"].map(String::from));
    } else {
        args.extend(
            [
                "--permission-mode",
                "acceptEdits",
                "--permission-prompts",
                "none",
            ]
            .map(String::from),
        );
    }
    for (flag, key, value) in [("--model", "model", model), ("--effort", "effort", effort)] {
        if !value.is_empty() {
            flag_value(key, value)?;
            args.extend([flag.to_string(), value.to_string()]);
        }
    }
    if let Some(turns) = request.max_turns.filter(|&n| n > 0) {
        args.extend(["--max-turns".to_string(), turns.to_string()]);
    }
    if let Some(schema) = request
        .json_schema
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        args.extend(["--json-schema".to_string(), schema.to_string()]);
    }
    // After `--` so a prompt that starts with a dash is still the prompt.
    args.push("--".to_string());
    args.push(request.prompt.clone());
    Ok(args)
}
