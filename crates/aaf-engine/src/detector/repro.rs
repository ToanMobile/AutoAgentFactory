//! Confirms a bug before it becomes a task: the repro command must fail on
//! every run. One failure out of two is a flaky test, not a confirmed bug.
//!
//! Security boundary:
//! - `commands.repro` is **not** executed automatically by `aaf run` or `aaf gate`.
//! - Untrusted CLI arguments, GitHub issue text, logs, or agent output must **never**
//!   be passed to [`ReproCommand::committed_shell`] (which invokes `/bin/sh -c`).
//! - Only [`ReproCommand::argv`] (direct argv execution without a shell) is intended
//!   for manual confirmation.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use crate::process::run_with_timeout;

pub const DEFAULT_RUNS: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Invocation {
    Argv { program: String, args: Vec<String> },
    Shell(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReproCommand {
    invocation: Invocation,
    cwd: PathBuf,
    timeout: Duration,
}

impl ReproCommand {
    /// Runs `program` with `args` directly. No shell is involved, so nothing
    /// in the arguments is expanded or interpreted.
    pub fn argv(
        program: impl Into<String>,
        args: Vec<String>,
        cwd: impl Into<PathBuf>,
        timeout: Duration,
    ) -> ReproCommand {
        ReproCommand {
            invocation: Invocation::Argv {
                program: program.into(),
                args,
            },
            cwd: cwd.into(),
            timeout,
        }
    }

    /// Runs `script` through `/bin/sh -c`. Only for a command string read
    /// from the repo's committed config (`.autofactory/config.toml`); never
    /// for untrusted CLI strings, text taken from an issue, a log, or agent
    /// output. `commands.repro` is not executed automatically by `aaf run` or
    /// `aaf gate` — use [`ReproCommand::argv`] for manual confirmation.
    pub fn committed_shell(
        script: impl Into<String>,
        cwd: impl Into<PathBuf>,
        timeout: Duration,
    ) -> ReproCommand {
        ReproCommand {
            invocation: Invocation::Shell(script.into()),
            cwd: cwd.into(),
            timeout,
        }
    }

    pub fn uses_shell(&self) -> bool {
        matches!(self.invocation, Invocation::Shell(_))
    }

    fn to_command(&self) -> Command {
        let mut command = match &self.invocation {
            Invocation::Argv { program, args } => {
                let mut command = Command::new(program);
                command.args(args);
                command
            }
            Invocation::Shell(script) => {
                let mut command = Command::new("/bin/sh");
                command.arg("-c").arg(script);
                command
            }
        };
        command.current_dir(&self.cwd);
        command
    }
}

/// Only `Confirmed` justifies creating a task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReproOutcome {
    /// Every run exited non-zero (or died from a signal).
    Confirmed,
    /// Some runs failed and some passed.
    Flaky,
    /// Every run exited zero.
    NotReproduced,
    /// A run hit the timeout and was killed. A hang is not proof of the
    /// reported failure, and later runs are skipped rather than paying the
    /// timeout again.
    TimedOut,
    /// The command could not be started, or `runs` was zero.
    Unrunnable { reason: String },
}

/// Runs `cmd` `runs` times in a row ([`DEFAULT_RUNS`] is the plan's 2/2),
/// each in its own process group with `cmd`'s timeout.
pub fn verify(cmd: &ReproCommand, runs: u32) -> ReproOutcome {
    if runs == 0 {
        return ReproOutcome::Unrunnable {
            reason: "zero runs requested".to_string(),
        };
    }
    let mut failed = 0u32;
    let mut passed = 0u32;
    for _ in 0..runs {
        match run_with_timeout(cmd.to_command(), cmd.timeout, false) {
            Err(err) => {
                return ReproOutcome::Unrunnable {
                    reason: err.to_string(),
                };
            }
            Ok(finished) => match finished.status {
                None => return ReproOutcome::TimedOut,
                Some(status) if status.success() => passed += 1,
                Some(_) => failed += 1,
            },
        }
    }
    match (failed, passed) {
        (_, 0) => ReproOutcome::Confirmed,
        (0, _) => ReproOutcome::NotReproduced,
        _ => ReproOutcome::Flaky,
    }
}
