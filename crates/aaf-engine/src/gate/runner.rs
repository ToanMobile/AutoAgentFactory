//! Runs the repo's own lint, build and test commands.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use aaf_domain::config::CommandsConfig;
use aaf_domain::gate::{CommandResult, GateMode};
use aaf_domain::ports::Clock;

use super::GateError;
use super::proc::capture;
use crate::throttle::{HeavyJob, Throttle, heavy_tool};

/// Bytes of combined output kept per command.
pub const LOG_TAIL_BYTES: usize = 4096;

/// Start of every receipt reason that records what the machine throttle did
/// (a wait, a pause, a degraded run, a leftover daemon). Such a note depends
/// on the load and the other gates of the moment, never on the failure, so
/// the unblocker's failure digest leaves these reasons out.
pub const THROTTLE_NOTE: &str = "throttle: ";

/// The test command the mode requires; empty means the gate cannot verify.
pub fn test_command(commands: &CommandsConfig, mode: GateMode) -> (&'static str, &str) {
    match mode {
        GateMode::Scoped => ("test_scoped", commands.test_scoped.trim()),
        GateMode::Full | GateMode::Release => ("test_full", commands.test_full.trim()),
    }
}

/// Configured commands for `mode`, in run order. Blank commands are left out.
pub fn plan(commands: &CommandsConfig, mode: GateMode) -> Vec<(&'static str, String)> {
    let mut steps = vec![("lint", commands.lint.trim())];
    if mode != GateMode::Scoped {
        steps.push(("build", commands.build.trim()));
    }
    steps.push(test_command(commands, mode));
    steps
        .into_iter()
        .filter(|(_, command)| !command.is_empty())
        .map(|(name, command)| (name, command.to_string()))
        .collect()
}

/// Runs one command through `/bin/sh -c` in the repo root, in its own process
/// group, outside the throttle. On timeout the whole group is killed.
pub fn run_command(
    repo_root: &Path,
    name: &str,
    command: &str,
    timeout: Duration,
) -> Result<CommandResult, GateError> {
    // By path: a bare `sh` is looked up in `PATH` at every spawn, where the
    // task can have put one of its own.
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(command).current_dir(repo_root);
    // When the gate itself runs inside a git hook these would point the
    // command's own git calls at the wrong repository.
    for var in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"] {
        cmd.env_remove(var);
    }
    let done = capture(cmd, timeout, Some(LOG_TAIL_BYTES), true)?;
    Ok(CommandResult {
        name: name.to_string(),
        command: command.to_string(),
        exit_code: if done.timed_out { None } else { done.exit_code },
        timed_out: done.timed_out,
        duration_ms: u64::try_from(done.duration.as_millis()).unwrap_or(u64::MAX),
        log_tail: String::from_utf8_lossy(&done.output).into_owned(),
    })
}

/// Runs the plan in order and stops at the first command that does not pass:
/// later steps would only add time to a result that is already `Blocked`.
pub fn run_all(
    repo_root: &Path,
    commands: &CommandsConfig,
    mode: GateMode,
    timeout: Duration,
) -> Result<Vec<CommandResult>, GateError> {
    let mut results = Vec::new();
    for (name, command) in plan(commands, mode) {
        let result = run_command(repo_root, name, &command, timeout)?;
        let passed = result.passed();
        results.push(result);
        if !passed {
            break;
        }
    }
    Ok(results)
}

/// Commands that go through the machine throttle (docs/PLAN.md C.11).
///
/// Build and test commands always do: whatever they start, ten of them at
/// once are what freezes the machine. Any other command (lint) stays light,
/// so cheap failures show before any queueing, unless its text starts a
/// build tool: `./gradlew lint` is a full Gradle run with a daemon, and the
/// name of the config key does not make it cheaper. The decision comes from
/// the one classifier the throttle, `aaf slot` and the doctor share.
pub fn is_heavy(name: &str, command: &str) -> bool {
    matches!(name, "build" | "test_scoped" | "test_full") || heavy_tool(command).is_some()
}

/// What the throttled commands need besides the plan itself.
pub struct HeavyContext<'a> {
    pub data_dir: &'a Path,
    /// Key of this repository in the learned memory statistics.
    pub stats_repo_id: &'a str,
    pub clock: &'a dyn Clock,
    pub throttle: &'a Throttle<'a>,
}

pub struct ThrottledRun {
    pub results: Vec<CommandResult>,
    /// What the throttle did (waits, degraded runs, pauses), for the
    /// receipt's `reasons`. Never changes the status.
    pub notes: Vec<String>,
}

fn run_heavy(
    repo_root: &Path,
    name: &str,
    command: &str,
    timeout: Duration,
    heavy: &HeavyContext<'_>,
    notes: &mut Vec<String>,
) -> Result<CommandResult, GateError> {
    let job = HeavyJob {
        repo_id: heavy.stats_repo_id,
        label: name,
        command,
        cwd: repo_root,
        timeout,
        log_tail_bytes: LOG_TAIL_BYTES,
    };
    let run = heavy
        .throttle
        .run_heavy(heavy.data_dir, heavy.clock, &job)?;
    if run.waited_secs > 0 {
        notes.push(format!(
            "{THROTTLE_NOTE}{name} waited {}s for a heavy slot or a calmer machine",
            run.waited_secs
        ));
    }
    if run.degraded {
        notes.push(format!(
            "{THROTTLE_NOTE}{name} ran degraded: CPU load stayed above {}% for {}s",
            heavy.throttle.machine.max_load_percent, heavy.throttle.machine.heavy_wait_secs
        ));
    }
    if !run.paused.is_zero() {
        notes.push(format!(
            "{THROTTLE_NOTE}{name} was paused {}s under critical memory pressure",
            run.paused.as_secs()
        ));
    }
    for daemon in &run.leftover_daemons {
        notes.push(format!(
            "{THROTTLE_NOTE}{} daemon still running pid={}; builds are not using --no-daemon",
            daemon.kind, daemon.pid
        ));
    }
    Ok(CommandResult {
        name: name.to_string(),
        command: command.to_string(),
        exit_code: if run.timed_out { None } else { run.exit_code },
        timed_out: run.timed_out,
        duration_ms: u64::try_from(run.duration.as_millis()).unwrap_or(u64::MAX),
        log_tail: String::from_utf8_lossy(&run.output).into_owned(),
    })
}

/// [`run_all`] with the heavy commands sent through the machine throttle.
pub fn run_all_throttled(
    repo_root: &Path,
    commands: &CommandsConfig,
    mode: GateMode,
    timeout: Duration,
    heavy: &HeavyContext<'_>,
) -> Result<ThrottledRun, GateError> {
    let mut run = ThrottledRun {
        results: Vec::new(),
        notes: Vec::new(),
    };
    for (name, command) in plan(commands, mode) {
        let result = if is_heavy(name, &command) {
            run_heavy(repo_root, name, &command, timeout, heavy, &mut run.notes)?
        } else {
            run_command(repo_root, name, &command, timeout)?
        };
        let passed = result.passed();
        run.results.push(result);
        if !passed {
            break;
        }
    }
    Ok(run)
}
