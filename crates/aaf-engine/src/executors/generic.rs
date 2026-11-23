//! Runs any agent CLI from the command template in `run.executor_cmd`.

use std::ffi::OsStr;
use std::path::Path;

use aaf_domain::ports::{AgentOutcome, AgentRequest, AgentRunner, PortError, PortResult};

use super::pty::{CommandSpec, GENERIC_TAIL_BYTES};
use super::{AccountPaths, ExecEnv, open_stream_log, run_confined_with_tail};

const PROMPT_PLACEHOLDER: &str = "{prompt}";

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

impl AgentRunner for GenericRunner {
    fn run(&self, request: &AgentRequest) -> PortResult<AgentOutcome> {
        // The layout of an arbitrary agent's config dir is not known: all of
        // it is open, nothing in it is singled out.
        let account = AccountPaths {
            writable: request
                .account
                .iter()
                .map(|account| account.config_dir.clone())
                .collect(),
            write_denied: Vec::new(),
        };
        let spec = self.command(request);
        let program = Path::new(&spec.program)
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or(&spec.program)
            .to_string();
        let mut log = open_stream_log(&self.env, request);
        log.write_start(&program, None);
        let output =
            run_confined_with_tail(spec, request, &self.env, account, None, GENERIC_TAIL_BYTES)?;
        log.write_generic_finish(&program, output.exit_code, output.timed_out, &output.tail);
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
