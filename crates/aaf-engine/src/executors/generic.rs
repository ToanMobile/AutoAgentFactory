//! Runs any agent CLI from the command template in `run.executor_cmd`.

use aaf_domain::ports::{AgentOutcome, AgentRequest, AgentRunner, PortError, PortResult};

use super::pty::CommandSpec;
use super::{AccountPaths, ExecEnv, run_confined};

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
        CommandSpec {
            program: self.template[0].clone(),
            args: self.template[1..]
                .iter()
                .map(|arg| arg.replace(PROMPT_PLACEHOLDER, &request.prompt))
                .collect(),
            cwd: None,
            env: Vec::new(),
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
        let output = run_confined(self.command(request), request, &self.env, account, None)?;
        Ok(AgentOutcome {
            exit_code: output.exit_code,
            timed_out: output.timed_out,
            // An arbitrary CLI has no structured answer; its output is it.
            final_text: output.tail.trim().to_string(),
            tokens_in: 0,
            tokens_out: 0,
            limit_hit: None,
            // An arbitrary CLI has no error vocabulary aaf knows; its
            // failures count against the task as they always did.
            infra: None,
            log_tail: output.tail,
        })
    }
}
