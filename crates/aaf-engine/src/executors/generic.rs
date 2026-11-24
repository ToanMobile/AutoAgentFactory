//! Runs any agent CLI from the command template in `run.executor_cmd`.

use std::ffi::OsStr;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use aaf_domain::account::ProviderKind;
use aaf_domain::ports::{
    AgentOutcome, AgentRequest, AgentRunner, InfraFailure, InfraKind, LimitHit, PortError,
    PortResult,
};
use serde_json::Value;

use super::pty::{CommandSpec, GENERIC_TAIL_BYTES};
use super::stream_log::StreamLog;
use super::{AccountPaths, ExecEnv, open_stream_log, run_confined_with_tail};

const PROMPT_PLACEHOLDER: &str = "{prompt}";
const RAW_ERROR_CHARS: usize = 500;

/// Entries of a Claude config dir closed for writing (matches `ClaudeCodeRunner::CONFIG_CLOSED`).
const CLAUDE_CONFIG_CLOSED: [&str; 3] = ["settings.json", "settings.local.json", "hooks"];

/// Entries of a Codex config directory that later runs execute or obey.
const CODEX_CONFIG_CLOSED: [&str; 7] = [
    "config.toml",
    "hooks.json",
    "AGENTS.md",
    "instructions.md",
    "rules",
    "skills",
    "plugins",
];

/// Extra Gemini / Antigravity config entries closed on top of [`super::agy::GEMINI_CONFIG_CLOSED`].
const GEMINI_EXTRA_CLOSED: [&str; 14] = [
    "settings.local.json",
    "hooks",
    "extensions",
    "policies",
    "commands",
    "antigravity",
    "antigravity-cli/bin",
    "antigravity-cli/mcp",
    "antigravity-cli/settings.json",
    "antigravity-ide/bin",
    "config/hooks.json",
    "config/mcp_config.json",
    "config/plugins",
    "config/node_modules",
];

/// Entries of an OpenCode config/state directory that later runs execute or obey.
const OPENCODE_CONFIG_CLOSED: [&str; 13] = [
    "opencode.json",
    "opencode.jsonc",
    "config.json",
    "plugin",
    "plugins",
    "tool",
    "tools",
    "agent",
    "agents",
    "command",
    "commands",
    "skill",
    "skills",
];

#[derive(Debug)]
pub struct GenericRunner {
    template: Vec<String>,
    env: ExecEnv,
}

impl GenericRunner {
    /// `template[0]` is the program; `{prompt}` in any later element is
    /// replaced by the prompt.
    pub fn new(template: Vec<String>, env: ExecEnv) -> PortResult<Self> {
        let Some(program) = template.first() else {
            return Err(PortError::Invalid("run.executor_cmd is empty".to_string()));
        };
        if program.is_empty() || program.contains(PROMPT_PLACEHOLDER) {
            return Err(PortError::Invalid(
                "run.executor_cmd must start with a fixed program".to_string(),
            ));
        }
        // An agent that never receives its task would only burn a run.
        if !template[1..]
            .iter()
            .any(|arg| arg.contains(PROMPT_PLACEHOLDER))
        {
            return Err(PortError::Invalid(format!(
                "run.executor_cmd has no {PROMPT_PLACEHOLDER} argument"
            )));
        }
        Ok(GenericRunner { template, env })
    }

    /// The exact command a run of `request` executes (before sandboxing).
    /// The prompt is substituted inside one argv element and no shell is
    /// involved, so nothing in it can add arguments or run commands.
    pub fn command(&self, request: &AgentRequest) -> CommandSpec {
        let program_name = Path::new(&self.template[0])
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or(&self.template[0]);
        let mut env = Vec::new();
        if program_name == "gemini" {
            // Headless Gemini CLI exits 55 in fresh worktrees under
            // ~/.cache/autofactory/worktrees unless workspace trust is enabled.
            env.push(("GEMINI_CLI_TRUST_WORKSPACE".to_string(), "true".to_string()));
        }
        let mut args: Vec<String> = self.template[1..]
            .iter()
            .map(|arg| arg.replace(PROMPT_PLACEHOLDER, &request.prompt))
            .collect();
        apply_sandbox_cli_flags(program_name, &self.template[1..], &mut args, request);
        CommandSpec {
            program: self.template[0].clone(),
            args,
            cwd: None,
            env,
            env_clear: false,
        }
    }

    /// Builds confined account/state paths for this runner (including default
    /// `~/.codex`, `~/.gemini`, or `~/.local/share/opencode` fallback when
    /// `request.account` is `None`).
    pub fn account_paths(&self, request: &AgentRequest) -> AccountPaths {
        let program_name = Path::new(&self.template[0])
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or(&self.template[0]);
        account_paths_for(request, program_name, self.env.home.as_deref())
    }
}

fn apply_sandbox_cli_flags(
    program_name: &str,
    template_args: &[String],
    args: &mut Vec<String>,
    request: &AgentRequest,
) {
    match program_name {
        "codex" => {
            let has_sandbox = template_args
                .iter()
                .any(|a| a == "--sandbox" || a == "-s" || a.starts_with("--sandbox="));
            let has_auto = template_args
                .iter()
                .any(|a| a == "--full-auto" || a == "--dangerously-bypass-approvals-and-sandbox");
            let idx = if args.first().map(String::as_str) == Some("exec") {
                1
            } else {
                args.iter().position(|a| a == "--").unwrap_or(0)
            };
            if request.sandbox.read_only && !has_sandbox {
                args.splice(idx..idx, ["--sandbox".to_string(), "read-only".to_string()]);
            } else if !request.sandbox.read_only
                && request.sandbox.enabled
                && !has_sandbox
                && !has_auto
            {
                args.insert(idx, "--full-auto".to_string());
            }
        }
        "gemini" => {
            let has_approval = template_args.iter().any(|a| {
                a == "--approval-mode"
                    || a.starts_with("--approval-mode=")
                    || a == "--yolo"
                    || a == "-y"
            });
            if !has_approval {
                if request.sandbox.read_only {
                    args.splice(0..0, ["--approval-mode".to_string(), "plan".to_string()]);
                } else if request.sandbox.enabled {
                    args.splice(0..0, ["--approval-mode".to_string(), "yolo".to_string()]);
                }
            }
        }
        _ => {}
    }
}

fn denied_under(root: &str, entries: &[&str]) -> Vec<String> {
    let base = root.trim_end_matches('/');
    entries
        .iter()
        .map(|name| format!("{base}/{name}"))
        .collect()
}

fn closed_dir_paths(config_dir: &str, entries: &[&str]) -> AccountPaths {
    AccountPaths {
        writable: vec![config_dir.to_string()],
        write_denied: denied_under(config_dir, entries),
        writable_regex: Vec::new(),
    }
}

fn gemini_account_paths(config_dir: &str, sandbox_enabled: bool) -> AccountPaths {
    let mut writable = Vec::with_capacity(2);
    for sub in ["tmp", "history"] {
        let dir = Path::new(config_dir).join(sub);
        if sandbox_enabled {
            let _ = std::fs::create_dir_all(&dir);
        }
        writable.push(dir.to_string_lossy().into_owned());
    }
    let mut write_denied = super::agy::closed_entries(config_dir);
    for extra in denied_under(config_dir, &GEMINI_EXTRA_CLOSED) {
        if !write_denied.contains(&extra) {
            write_denied.push(extra);
        }
    }
    AccountPaths {
        writable,
        write_denied,
        writable_regex: Vec::new(),
    }
}

/// Builds confined account paths for a generic CLI run from `request.account`,
/// or falls back to default home state dirs based on `program_name` when `None`.
pub fn account_paths_for(
    request: &AgentRequest,
    program_name: &str,
    home: Option<&str>,
) -> AccountPaths {
    if let Some(account) = &request.account {
        let dir = &account.config_dir;
        return match account.provider {
            ProviderKind::ClaudeCode => closed_dir_paths(dir, &CLAUDE_CONFIG_CLOSED),
            ProviderKind::Codex => closed_dir_paths(dir, &CODEX_CONFIG_CLOSED),
            ProviderKind::Antigravity => gemini_account_paths(dir, request.sandbox.enabled),
            ProviderKind::OpenCode => closed_dir_paths(dir, &OPENCODE_CONFIG_CLOSED),
        };
    }
    let Some(home) = home
        .filter(|h| !h.is_empty())
        .map(|h| h.trim_end_matches('/'))
    else {
        return AccountPaths::default();
    };
    let at = |sub: &str| format!("{home}/{sub}");
    match program_name {
        "claude" => AccountPaths {
            writable: vec![at(".claude"), at(".claude.json")],
            write_denied: denied_under(&at(".claude"), &CLAUDE_CONFIG_CLOSED),
            writable_regex: Vec::new(),
        },
        "codex" => closed_dir_paths(&at(".codex"), &CODEX_CONFIG_CLOSED),
        "gemini" | "agy" | "antigravity" => {
            gemini_account_paths(&at(".gemini"), request.sandbox.enabled)
        }
        "opencode" => closed_dir_paths(&at(".local/share/opencode"), &OPENCODE_CONFIG_CLOSED),
        _ => AccountPaths::default(),
    }
}

/// Builds the confined account paths for a generic CLI run with an explicit account.
pub fn account_paths(request: &AgentRequest) -> AccountPaths {
    account_paths_for(request, "", None)
}

fn extract_target_hint(val: &Value) -> Option<String> {
    const KEYS: [&str; 6] = ["command", "file_path", "path", "pattern", "url", "query"];
    let obj = val.as_object()?;
    for key in KEYS {
        if let Some(text) = obj.get(key).and_then(Value::as_str).map(str::trim)
            && !text.is_empty()
        {
            return Some(text.to_string());
        }
    }
    None
}

/// Projects structured JSONL events emitted by non-Claude executors (Codex
/// `--json`, Gemini `--output-format stream-json`, or Claude-compatible
/// streams) into bounded, secret-scrubbed [`StreamLog`] entries.
///
/// Plain non-JSON lines are ignored so raw terminal output is never dumped to
/// the live stream projection.
fn record_generic_line(log: &mut StreamLog, line: &str) {
    let Ok(Value::Object(event)) = serde_json::from_str::<Value>(line.trim()) else {
        return;
    };
    match event.get("type").and_then(Value::as_str) {
        Some("assistant" | "result" | "error" | "rate_limit_event") => {
            log.record_claude_line(line);
        }
        Some("tool_use") => {
            let name = event
                .get("tool_name")
                .or_else(|| event.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let target = event
                .get("parameters")
                .or_else(|| event.get("input"))
                .and_then(extract_target_hint);
            log.write_event("tool_use", Some(name), target.as_deref());
        }
        Some("message") => {
            let role = event.get("role").and_then(Value::as_str);
            if role == Some("assistant")
                && let Some(text) = event.get("content").and_then(Value::as_str).map(str::trim)
                && !text.is_empty()
            {
                log.write_event("assistant", None, Some(text));
            }
        }
        Some(kind @ ("item.started" | "item.completed")) => {
            let Some(item) = event.get("item").and_then(Value::as_object) else {
                return;
            };
            match (kind, item.get("type").and_then(Value::as_str)) {
                ("item.started", Some("command_execution")) => {
                    let cmd = item.get("command").and_then(Value::as_str);
                    log.write_event("tool_use", Some("shell"), cmd);
                }
                ("item.started", Some("mcp_tool_call")) => {
                    let tool = item
                        .get("tool")
                        .or_else(|| item.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("mcp");
                    let target = item.get("arguments").and_then(extract_target_hint);
                    log.write_event("tool_use", Some(tool), target.as_deref());
                }
                ("item.completed", Some("agent_message")) => {
                    if let Some(text) = item.get("text").and_then(Value::as_str).map(str::trim)
                        && !text.is_empty()
                    {
                        log.write_event("assistant", None, Some(text));
                    }
                }
                ("item.completed", Some("file_change")) => {
                    let path = item.get("path").and_then(Value::as_str);
                    log.write_event("tool_use", Some("file_change"), path);
                }
                _ => {}
            }
        }
        Some("turn.failed") => {
            let msg = event
                .get("error")
                .and_then(|e| {
                    e.get("message")
                        .and_then(Value::as_str)
                        .or_else(|| e.as_str())
                })
                .unwrap_or("turn failed");
            log.write_event("error", None, Some(msg));
        }
        _ => {}
    }
}

impl AgentRunner for GenericRunner {
    fn run(&self, request: &AgentRequest) -> PortResult<AgentOutcome> {
        let account = self.account_paths(request);
        let spec = self.command(request);
        let program = Path::new(&spec.program)
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or(&spec.program)
            .to_string();
        let mut log = open_stream_log(&self.env, request);
        log.write_start(&program, None);
        let log = Arc::new(Mutex::new(log));
        let feed = Arc::clone(&log);
        let output = run_confined_with_tail(
            spec,
            request,
            &self.env,
            account,
            &program,
            Some(Box::new(move |line| {
                let mut guard = feed.lock().unwrap_or_else(PoisonError::into_inner);
                record_generic_line(&mut guard, line);
            })),
            GENERIC_TAIL_BYTES,
        )?;
        let mut guard = log.lock().unwrap_or_else(PoisonError::into_inner);
        guard.write_generic_finish(&program, output.exit_code, output.timed_out, &output.tail);
        let (limit_hit, infra) =
            classify_generic_failure(output.exit_code, output.timed_out, &output.tail);
        Ok(AgentOutcome {
            exit_code: output.exit_code,
            timed_out: output.timed_out,
            // An arbitrary CLI has no structured answer; strip trailing `[aaf]` notices.
            final_text: strip_aaf_notices(&output.tail),
            tokens_in: 0,
            tokens_out: 0,
            limit_hit,
            infra,
            log_tail: output.tail,
        })
    }

    fn reports_tokens(&self) -> bool {
        false
    }
}

const LIMIT_NEEDLES: [&str; 15] = [
    "http 429",
    "status 429",
    "status: 429",
    "error 429",
    "429 too many requests",
    "too many requests",
    "rate_limit_exceeded",
    "rate limit exceeded",
    "rate_limit",
    "rate limit",
    "resource_exhausted",
    "quota exceeded",
    "insufficient_quota",
    "usage limit reached",
    "exceeded your current quota",
];

const AUTH_NEEDLES: [&str; 19] = [
    "http 401",
    "status 401",
    "status: 401",
    "error 401",
    "401 unauthorized",
    "http 403",
    "status 403",
    "status: 403",
    "403 forbidden",
    "unauthenticated",
    "unauthorized",
    "authentication_error",
    "authentication failed",
    "invalid_api_key",
    "invalid api key",
    "invalid credentials",
    "invalid_grant",
    "not logged in",
    "login required",
];

const NET_NEEDLES: [&str; 22] = [
    "http 500",
    "http 502",
    "http 503",
    "http 504",
    "status 500",
    "status 502",
    "status 503",
    "status 504",
    "502 bad gateway",
    "503 service unavailable",
    "504 gateway timeout",
    "bad gateway",
    "service unavailable",
    "gateway timeout",
    "overloaded_error",
    "server_error",
    "connection refused",
    "connection reset",
    "econnrefused",
    "econnreset",
    "could not resolve host",
    "network unreachable",
];

/// Classifies a failed generic CLI run's output tail into a quota/rate-limit
/// hit or an auth/network infrastructure failure. Returns `(None, None)` for
/// successful or timed-out runs.
pub fn classify_generic_failure(
    exit_code: Option<i32>,
    timed_out: bool,
    tail: &str,
) -> (Option<LimitHit>, Option<InfraFailure>) {
    if timed_out || exit_code == Some(0) {
        return (None, None);
    }
    let cleaned = strip_aaf_notices(&strip_ansi_basic(tail));
    let raw: String = cleaned.chars().take(RAW_ERROR_CHARS).collect();
    let json_status = extract_json_error_status(&cleaned);
    let lower = cleaned.to_ascii_lowercase();
    if json_status == Some(429) || LIMIT_NEEDLES.iter().any(|n| lower.contains(n)) {
        return (
            Some(LimitHit {
                reset_at: None,
                raw,
            }),
            None,
        );
    }
    if matches!(json_status, Some(401 | 403)) || AUTH_NEEDLES.iter().any(|n| lower.contains(n)) {
        return (
            None,
            Some(InfraFailure {
                kind: InfraKind::Auth,
                raw,
            }),
        );
    }
    if matches!(json_status, Some(500..=599)) || NET_NEEDLES.iter().any(|n| lower.contains(n)) {
        return (
            None,
            Some(InfraFailure {
                kind: InfraKind::Network,
                raw,
            }),
        );
    }
    (None, None)
}

fn extract_json_error_status(text: &str) -> Option<i64> {
    for line in text.lines().rev() {
        let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let status = obj
            .get("status_code")
            .or_else(|| obj.get("status"))
            .or_else(|| obj.get("code"))
            .and_then(Value::as_i64)
            .or_else(|| {
                obj.get("error")
                    .and_then(Value::as_object)
                    .and_then(|e| {
                        e.get("status_code")
                            .or_else(|| e.get("status"))
                            .or_else(|| e.get("code"))
                    })
                    .and_then(Value::as_i64)
            });
        if status.is_some() {
            return status;
        }
    }
    None
}

fn strip_ansi_basic(input: &str) -> String {
    if !input.contains('\x1b') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    let _ = chars.by_ref().find(|n| ('@'..='~').contains(n));
                }
                Some(']') => {
                    chars.next();
                    while let Some(n) = chars.next() {
                        if n == '\x07' || (n == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else if c != '\r' {
            out.push(c);
        }
    }
    out
}

/// Drops trailing `[aaf] ...` diagnostic lines appended after the child exited.
fn strip_aaf_notices(tail: &str) -> String {
    let mut lines: Vec<&str> = tail.lines().collect();
    while let Some(&last) = lines.last() {
        let trimmed = last.trim();
        if trimmed.is_empty() || trimmed.starts_with("[aaf] ") {
            lines.pop();
        } else {
            break;
        }
    }
    lines.join("\n").trim().to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
