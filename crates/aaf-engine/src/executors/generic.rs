//! Runs any agent CLI from the command template in `run.executor_cmd`.

use std::ffi::OsStr;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use aaf_domain::account::ProviderKind;
use aaf_domain::ports::{AgentOutcome, AgentRequest, AgentRunner, PortError, PortResult};
use serde_json::Value;

use super::pty::{CommandSpec, GENERIC_TAIL_BYTES};
use super::stream_log::StreamLog;
use super::{AccountPaths, ExecEnv, open_stream_log, run_confined_with_tail};

const PROMPT_PLACEHOLDER: &str = "{prompt}";

/// Entries of a Codex config directory that later runs execute or obey
/// (matching `ClaudeCodeRunner::CONFIG_CLOSED`).
const CODEX_CONFIG_CLOSED: [&str; 2] = ["config.toml", "hooks.json"];

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
        CommandSpec {
            program: self.template[0].clone(),
            args: self.template[1..]
                .iter()
                .map(|arg| arg.replace(PROMPT_PLACEHOLDER, &request.prompt))
                .collect(),
            cwd: None,
            env,
            env_clear: false,
        }
    }
}

fn account_paths(request: &AgentRequest) -> AccountPaths {
    let Some(account) = &request.account else {
        return AccountPaths::default();
    };
    let write_denied = match account.provider {
        ProviderKind::Codex => CODEX_CONFIG_CLOSED
            .iter()
            .map(|name| format!("{}/{name}", account.config_dir))
            .collect(),
        _ => Vec::new(),
    };
    AccountPaths {
        writable: vec![account.config_dir.clone()],
        write_denied,
    }
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
        let account = account_paths(request);
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
            Some(Box::new(move |line| {
                let mut guard = feed.lock().unwrap_or_else(PoisonError::into_inner);
                record_generic_line(&mut guard, line);
            })),
            GENERIC_TAIL_BYTES,
        )?;
        let mut guard = log.lock().unwrap_or_else(PoisonError::into_inner);
        guard.write_generic_finish(&program, output.exit_code, output.timed_out, &output.tail);
        Ok(AgentOutcome {
            exit_code: output.exit_code,
            timed_out: output.timed_out,
            // An arbitrary CLI has no structured answer; strip trailing `[aaf]` notices.
            final_text: strip_aaf_notices(&output.tail),
            tokens_in: 0,
            tokens_out: 0,
            limit_hit: None,
            // An arbitrary CLI has no error vocabulary aaf knows; its
            // failures count against the task as they always did.
            infra: None,
            log_tail: output.tail,
        })
    }

    fn reports_tokens(&self) -> bool {
        false
    }
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
mod tests {
    use aaf_domain::ports::SandboxPolicy;

    use super::*;

    #[test]
    fn gemini_command_sets_headless_workspace_trust_env() {
        let req = AgentRequest {
            run_id: "r1".to_string(),
            cwd: "/tmp".to_string(),
            prompt: "audit".to_string(),
            account: None,
            timeout_secs: 60,
            sandbox: SandboxPolicy::default(),
        };
        let runner = GenericRunner::new(
            vec![
                "gemini".to_string(),
                "-p".to_string(),
                "{prompt}".to_string(),
            ],
            ExecEnv::default(),
        )
        .unwrap();
        let spec = runner.command(&req);
        assert!(
            spec.env
                .contains(&("GEMINI_CLI_TRUST_WORKSPACE".to_string(), "true".to_string())),
            "{:?}",
            spec.env
        );
    }
}
