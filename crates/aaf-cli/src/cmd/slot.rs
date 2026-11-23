//! `aaf slot [--label <name>] [--timeout <secs>] -- <command> [args...]`:
//! run any command through the machine-wide queue of heavy commands that
//! `aaf gate` uses (docs/PLAN.md C.11), so a git hook, a CI step or an agent
//! that starts a build does not start it beside the others.
//!
//! The command's stdout and stderr are the caller's own; `aaf` writes only
//! progress lines to stderr. The exit code is the command's: its own code,
//! or 128 + the signal that killed it. `aaf` itself exits 75 when
//! `--timeout` passed while waiting (the command was never started), 126 or
//! 127 when the command could not be started, 1 for its own errors and 2
//! for a usage error. Works outside a git repository.
//!
//! A TERM, INT or HUP that ends `aaf`, sent to its terminal's process group
//! or to the `aaf` process alone, also ends the command's process group
//! (forwarded, then SIGKILL after a grace period; see the engine's signal
//! relay), and `aaf` ends by that signal.

use std::ffi::OsString;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, ExitStatus};
use std::time::Duration;

use aaf_engine::throttle::{
    Progress, SLOT_ENV, SlotJob, SystemProbe, Throttle, daemons, inherited_slot,
};
use serde_json::json;

use crate::cmd::gate::progress_line;
use crate::ctx::{Ctx, SystemClock};
use crate::output::{CmdError, CmdOutput, CmdResult, notice};

/// `EX_TEMPFAIL`: no slot within `--timeout`; trying again later can work.
pub const EXIT_NO_SLOT: i32 = 75;
/// Standard `timeout(1)` exit code when `--exec-timeout` expires while running.
pub const EXIT_EXEC_TIMEOUT: i32 = 124;
const EXIT_CANNOT_RUN: i32 = 126;
const EXIT_NOT_FOUND: i32 = 127;

#[derive(Debug, clap::Args)]
pub struct SlotArgs {
    /// Name the command is measured under; default: its first word.
    #[arg(long, value_name = "NAME")]
    label: Option<String>,
    /// Give up with exit 75 when no slot was granted after this long.
    #[arg(long, value_name = "SECS")]
    timeout: Option<u64>,
    /// Kill the command's process group with exit 124 if it runs longer than this.
    #[arg(long, value_name = "SECS")]
    exec_timeout: Option<u64>,
    /// The command and its arguments, after `--`.
    #[arg(last = true, required = true, value_name = "COMMAND")]
    command: Vec<OsString>,
}

/// True when `--json` stands among aaf's own arguments (before `--`).
fn json_requested(args: impl Iterator<Item = OsString>) -> bool {
    args.take_while(|arg| arg != "--")
        .any(|arg| arg == "--json")
}

fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

/// The slot an ancestor of this process holds, when it still does. Waiting
/// for a slot below the command that holds one would never end (a hook
/// fired by a gate's test command, a build script that calls `aaf slot`).
/// The variable must carry the token of the run that holds the slot now:
/// the slot number alone, a made-up token or the token of a run that is
/// over opens nothing, and the command takes its turn.
fn slot_of_ancestor(ctx: &Ctx) -> Option<usize> {
    let (_, value) = ctx.aaf_env.iter().find(|(key, _)| key == SLOT_ENV)?;
    inherited_slot(&ctx.data_dir, value)
}

/// Runs the command below the holder of `slot`, without a slot of its own.
///
/// It gets the daemon switches again, as whatever stands between the holder
/// and this process may have replaced `GRADLE_OPTS`. Priority and watchdog
/// are not set up a second time because the holder's still apply: the nice
/// value and the background policy are inherited from the holder's command,
/// and this command stays in that command's process group, which is what
/// the holder's watchdog measures and stops under memory pressure.
fn run_nested(args: &SlotArgs, program: &OsString, label: &str) -> Result<i32, CmdError> {
    let gradle_opts = daemons::gradle_opts(std::env::var_os("GRADLE_OPTS").as_deref());
    let status = Command::new(program)
        .args(&args.command[1..])
        .env("GRADLE_OPTS", gradle_opts)
        .status();
    match status {
        Ok(status) => Ok(exit_code(status)),
        Err(err) => start_failure(label, &err),
    }
}

fn start_failure(label: &str, err: &io::Error) -> Result<i32, CmdError> {
    let code = match err.kind() {
        io::ErrorKind::NotFound => EXIT_NOT_FOUND,
        io::ErrorKind::PermissionDenied => EXIT_CANNOT_RUN,
        _ => return Err(CmdError::new(format!("{label}: {err}"))),
    };
    notice("slot", &format!("{label}: cannot start the command: {err}"));
    Ok(code)
}

fn run_command(args: &SlotArgs, ctx: &Ctx) -> Result<i32, CmdError> {
    let Some(program) = args.command.first() else {
        return Err(CmdError::new("no command given"));
    };
    let label = args
        .label
        .clone()
        .unwrap_or_else(|| program.to_string_lossy().into_owned());

    if let Some(slot) = slot_of_ancestor(ctx) {
        notice(
            "slot",
            &format!("{label}: already inside heavy slot {slot}; running at once"),
        );
        return run_nested(args, program, &label);
    }

    let repo_root = ctx.repo_root().ok();
    // Outside a repo the machine limits of tier 1 still apply.
    let cfg = ctx.load_config(repo_root.as_deref().unwrap_or(&ctx.cwd))?;
    let repo_id = match &repo_root {
        Some(root) => aaf_engine::gate::stats_repo_id(root),
        None => aaf_fp::repo_id(&ctx.cwd),
    };
    let progress = |event: &Progress<'_>| notice("slot", &progress_line(event));
    let probe = SystemProbe::detect();
    let throttle = Throttle::system(&cfg.global.machine, &probe, &progress);
    let job = SlotJob {
        repo_id: &repo_id,
        label: &label,
        argv: &args.command,
        cwd: &ctx.cwd,
        wait_limit_secs: args.timeout,
        exec_timeout: args.exec_timeout.map(Duration::from_secs),
        inherit_stdin: !ctx.stdin_is_tty,
    };
    match throttle.run_slot(&ctx.data_dir, &SystemClock, &job) {
        Ok(Some(run)) => {
            for daemon in &run.leftover_daemons {
                notice(
                    "slot",
                    &format!(
                        "{label}: {} daemon still running pid={}; \
                         the build is not using --no-daemon",
                        daemon.kind, daemon.pid
                    ),
                );
            }
            if run.timed_out {
                notice(
                    "slot",
                    &format!(
                        "{label}: command timed out after {}s and was terminated",
                        args.exec_timeout.unwrap_or(0)
                    ),
                );
                return Ok(EXIT_EXEC_TIMEOUT);
            }
            Ok(exit_code(run.status))
        }
        Ok(None) => {
            notice(
                "slot",
                &format!(
                    "{label}: no heavy slot within {}s; the command was not started",
                    args.timeout.unwrap_or(0)
                ),
            );
            Ok(EXIT_NO_SLOT)
        }
        Err(err) => start_failure(&label, &err),
    }
}

pub fn run(args: &SlotArgs, ctx: &Ctx) -> CmdResult {
    if json_requested(std::env::args_os()) {
        // stdout belongs to the command, so there is no object to print it
        // into; refusing is clearer than a flag that does nothing.
        return Ok(CmdOutput::policy("usage_error", "").field(
            "error",
            json!("aaf slot does not take --json: stdout belongs to the command it runs"),
        ));
    }
    let code = run_command(args, ctx)?;
    // The command's code is passed on as it is; the caller of `run` can
    // only express 0, 1 and 2, and would print a result line of its own.
    std::process::exit(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> impl Iterator<Item = OsString> {
        words
            .iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn json_counts_only_among_the_arguments_of_aaf() {
        assert!(json_requested(args(&[
            "aaf", "--json", "slot", "--", "true"
        ])));
        assert!(json_requested(args(&[
            "aaf", "slot", "--json", "--", "true"
        ])));
        assert!(!json_requested(args(&[
            "aaf", "slot", "--", "tool", "--json"
        ])));
        assert!(!json_requested(args(&["aaf", "slot", "--", "true"])));
    }
}
