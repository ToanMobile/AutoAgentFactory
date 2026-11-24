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
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use aaf_engine::throttle::{
    Progress, SLOT_ENV, SignalRelay, SlotJob, SystemProbe, Throttle, daemons, inherited_slot,
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

/// `/bin/kill <args>`; true when it exited 0.
fn kill(args: &[&str]) -> bool {
    Command::new("/bin/kill")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// True when `ps -A -o pgid=,stat=` output lists a member of `pgid` that is
/// not a zombie. Output with no line at all proves nothing: true.
fn ps_lists_live_member(text: &str, pgid: u32) -> bool {
    if text.trim().is_empty() {
        return true;
    }
    let target = pgid.to_string();
    text.lines().any(|line| {
        let mut cols = line.split_whitespace();
        match (cols.next(), cols.next()) {
            (Some(group), Some(stat)) => group == target && !stat.starts_with('Z'),
            _ => false,
        }
    })
}

/// True when process group `pgid` still has a member that is not a zombie.
/// On Linux `kill -0 -- -<pgid>` goes on succeeding while the leader is an
/// unreaped zombie, which held the kill loop below for its whole bound
/// (`/bin/kill` run every 10 ms); macOS already reports the group empty.
/// Fails safe (true) when `ps` cannot answer.
fn group_has_live_member(pgid: u32) -> bool {
    if !kill(&["-0", "--", &format!("-{pgid}")]) {
        return false;
    }
    let Ok(out) = Command::new("/bin/ps")
        .args(["-A", "-o", "pgid=,stat="])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return true;
    };
    !out.status.success() || ps_lists_live_member(&String::from_utf8_lossy(&out.stdout), pgid)
}

/// SIGKILL to the group of `pgid` until no live member is left, for at most
/// five seconds. Called before the leader is reaped, so the group id cannot
/// belong to anyone else yet.
fn kill_process_group(pgid: u32) {
    let group = format!("-{pgid}");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        kill(&["-KILL", "--", &group]);
        if !group_has_live_member(pgid) || Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_nested(
    child: &mut Child,
    started: Instant,
    timeout_secs: u64,
    label: &str,
) -> Result<i32, CmdError> {
    let limit = Duration::from_secs(timeout_secs);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(exit_code(status)),
            Ok(None) => {}
            Err(err) => {
                kill_process_group(child.id());
                let _ = child.wait();
                return Err(CmdError::new(format!("{label}: {err}")));
            }
        }
        if started.elapsed() >= limit {
            kill_process_group(child.id());
            let _ = child.kill();
            let _ = child.wait();
            notice(
                "slot",
                &format!("{label}: command timed out after {timeout_secs}s and was terminated"),
            );
            return Ok(EXIT_EXEC_TIMEOUT);
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Runs the command below the holder of `slot`, without a slot of its own.
///
/// It gets the daemon switches again, as whatever stands between the holder
/// and this process may have replaced `GRADLE_OPTS`. Priority and watchdog
/// are not set up a second time because the holder's still apply: the nice
/// value and the background policy are inherited from the holder's command,
/// and when `--exec-timeout` is not set this command stays in that command's
/// process group, which is what the holder's watchdog measures and stops
/// under memory pressure.
fn run_nested(
    args: &SlotArgs,
    program: &OsString,
    label: &str,
    inherit_stdin: bool,
) -> Result<i32, CmdError> {
    let gradle_opts = daemons::gradle_opts(std::env::var_os("GRADLE_OPTS").as_deref());
    let Some(timeout_secs) = args.exec_timeout else {
        let status = Command::new(program)
            .args(&args.command[1..])
            .env("GRADLE_OPTS", gradle_opts)
            .status();
        return match status {
            Ok(status) => Ok(exit_code(status)),
            Err(err) => start_failure(label, &err),
        };
    };
    let mut cmd = Command::new(program);
    cmd.args(&args.command[1..])
        .env("GRADLE_OPTS", gradle_opts)
        .process_group(0);
    if !inherit_stdin {
        cmd.stdin(Stdio::null());
    }
    let started = Instant::now();
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => return start_failure(label, &err),
    };
    let relay = SignalRelay::start(child.id()).ok();
    let result = wait_nested(&mut child, started, timeout_secs, label);
    if let Some(r) = relay {
        r.stop();
    }
    result
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
        return run_nested(args, program, &label, !ctx.stdin_is_tty);
    }

    // One git call for the work tree and the id its siblings share.
    let (repo_root, repo_id) = match aaf_engine::gate::repo_root_and_stats_id(&ctx.cwd) {
        Some((root, id)) => (Some(root), id),
        None => (None, aaf_fp::repo_id(&ctx.cwd)),
    };
    // Outside a repo the machine limits of tier 1 still apply.
    let cfg = ctx.load_config(repo_root.as_deref().unwrap_or(&ctx.cwd))?;
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
#[allow(clippy::unwrap_used, clippy::expect_used)]
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

    #[test]
    fn ps_listing_counts_only_live_members_of_the_group() {
        assert!(!ps_lists_live_member(" 4242 Z+\n  100 S\n", 4242));
        assert!(!ps_lists_live_member(" 4242 Zs\n42420 R\n", 4242));
        assert!(ps_lists_live_member(" 4242 Z+\n 4242 S+\n", 4242));
        assert!(ps_lists_live_member(" 4242 R\n", 4242));
        assert!(!ps_lists_live_member("  100 S\n", 4242));
        // Nothing to read is no proof that the group is gone.
        assert!(ps_lists_live_member("", 4242));
        assert!(ps_lists_live_member("\n  \n", 4242));
    }

    /// The leader has exited and is not reaped yet: a zombie, the only
    /// member of its group. Killing the group returns at once instead of
    /// spinning for the whole bound (on Linux `kill -0` still answers yes).
    #[test]
    fn killing_a_group_whose_only_member_is_a_zombie_leader_returns_at_once() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .process_group(0)
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let pgid = child.id();
        // Exited, not reaped.
        let settle = Instant::now();
        while kill(&["-0", "--", &pgid.to_string()])
            && ps_lists_live_member(
                &String::from_utf8_lossy(
                    &Command::new("/bin/ps")
                        .args(["-A", "-o", "pgid=,stat="])
                        .output()
                        .unwrap()
                        .stdout,
                ),
                pgid,
            )
            && settle.elapsed() < Duration::from_secs(5)
        {
            thread::sleep(Duration::from_millis(10));
        }
        let started = Instant::now();
        kill_process_group(pgid);
        let took = started.elapsed();
        let _ = child.wait();
        assert!(took < Duration::from_secs(2), "took {took:?}");
    }

    #[test]
    fn run_nested_enforces_exec_timeout() {
        let program = OsString::from("/bin/sh");
        let timed_out_args = SlotArgs {
            label: Some("sleep-test".into()),
            timeout: None,
            exec_timeout: Some(1),
            command: vec![
                program.clone(),
                OsString::from("-c"),
                OsString::from("sleep 10"),
            ],
        };
        let code = run_nested(&timed_out_args, &program, "sleep-test", false).unwrap();
        assert_eq!(code, EXIT_EXEC_TIMEOUT);

        let fast_args = SlotArgs {
            label: Some("fast-test".into()),
            timeout: None,
            exec_timeout: Some(5),
            command: vec![
                program.clone(),
                OsString::from("-c"),
                OsString::from("exit 0"),
            ],
        };
        let ok = run_nested(&fast_args, &program, "fast-test", false).unwrap();
        assert_eq!(ok, 0);
    }
}
