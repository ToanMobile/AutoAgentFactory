//! Confirms a bug before it becomes a task: the repro command must fail on
//! every run. One failure out of two is a flaky test, not a confirmed bug.
//!
//! Security boundary:
//! - `commands.repro` is **not** executed automatically by `aaf run` or `aaf gate`.
//! - Untrusted CLI arguments, GitHub issue text, logs, or agent output must **never**
//!   be passed to [`ReproCommand::committed_shell`] (which invokes `/bin/sh -c`).
//! - Only [`ReproCommand::argv`] (direct argv execution without a shell) is intended
//!   for manual confirmation.

use std::path::{Path, PathBuf};
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

    /// The script an interpreter would run (`sh missing.sh`) when that file
    /// does not exist. Shells disagree on the exit code of a missing script
    /// (bash 127, dash 2), so the failure of such a run cannot tell a typo
    /// from the bug: it is checked before any run.
    fn missing_script(&self) -> Option<String> {
        const INTERPRETERS: &[&str] = &[
            "sh", "bash", "dash", "zsh", "ksh", "python", "python3", "node", "ruby", "perl",
        ];
        const INLINE: &[&str] = &["-c", "-e", "-E", "-m", "-p", "-r", "--eval", "--print"];
        let Invocation::Argv { program, args } = &self.invocation else {
            return None;
        };
        let name = Path::new(program).file_name()?.to_str()?;
        if !INTERPRETERS.contains(&name) {
            return None;
        }
        for arg in args {
            if INLINE.contains(&arg.as_str()) || arg == "-" {
                return None;
            }
            if arg.starts_with('-') {
                continue;
            }
            let path = self.cwd.join(arg);
            return (!path.exists()).then(|| format!("script not found: {arg}"));
        }
        None
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
    /// The command could not be started, a run exited 126/127 (the shell's
    /// "not executable" / "command not found"), or `runs` was zero.
    Unrunnable { reason: String },
}

/// Lines of stderr kept from the last failing run.
pub const STDERR_TAIL_LINES: usize = 10;

/// Characters kept per stderr line, so one huge line cannot flood the report.
const STDERR_LINE_CHARS: usize = 500;

/// How the last failing run ended.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FailureDetail {
    /// `None` when the run died from a signal.
    pub exit_code: Option<i32>,
    /// The last [`STDERR_TAIL_LINES`] lines of its stderr.
    pub stderr_tail: Vec<String>,
}

/// [`ReproOutcome`] plus why the last failing run failed, if any run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReproReport {
    pub outcome: ReproOutcome,
    pub last_failure: Option<FailureDetail>,
}

fn stderr_tail(stderr: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(STDERR_TAIL_LINES);
    lines[start..]
        .iter()
        .map(|line| line.chars().take(STDERR_LINE_CHARS).collect())
        .collect()
}

/// Runs `cmd` `runs` times in a row ([`DEFAULT_RUNS`] is the plan's 2/2),
/// each in its own process group with `cmd`'s timeout.
pub fn verify(cmd: &ReproCommand, runs: u32) -> ReproOutcome {
    verify_report(cmd, runs).outcome
}

/// [`verify`], also returning the exit code and stderr tail of the last
/// failing run so the operator can see *why* it failed.
pub fn verify_report(cmd: &ReproCommand, runs: u32) -> ReproReport {
    let report = |outcome, last_failure| ReproReport {
        outcome,
        last_failure,
    };
    if runs == 0 {
        return report(
            ReproOutcome::Unrunnable {
                reason: "zero runs requested".to_string(),
            },
            None,
        );
    }
    if let Some(reason) = cmd.missing_script() {
        return report(ReproOutcome::Unrunnable { reason }, None);
    }
    let mut failed = 0u32;
    let mut passed = 0u32;
    let mut last_failure = None;
    for _ in 0..runs {
        let finished = match run_with_timeout(cmd.to_command(), cmd.timeout, true) {
            Err(err) => {
                let reason = err.to_string();
                return report(ReproOutcome::Unrunnable { reason }, last_failure);
            }
            Ok(finished) => finished,
        };
        let Some(status) = finished.status else {
            return report(ReproOutcome::TimedOut, last_failure);
        };
        if status.success() {
            passed += 1;
            continue;
        }
        let detail = FailureDetail {
            exit_code: status.code(),
            stderr_tail: stderr_tail(&finished.stderr),
        };
        // POSIX shells and `env`/`npx`-style wrappers exit 127 when the
        // program is missing and 126 when it cannot be executed: the
        // command never ran, so its failure proves nothing about the bug.
        let never_ran = match status.code() {
            Some(127) => Some("exit 127: command not found / not executable"),
            Some(126) => Some("exit 126: command not executable"),
            _ => None,
        };
        if let Some(reason) = never_ran {
            let reason = reason.to_string();
            return report(ReproOutcome::Unrunnable { reason }, Some(detail));
        }
        failed += 1;
        last_failure = Some(detail);
    }
    let outcome = match (failed, passed) {
        (_, 0) => ReproOutcome::Confirmed,
        (0, _) => ReproOutcome::NotReproduced,
        _ => ReproOutcome::Flaky,
    };
    report(outcome, last_failure)
}
