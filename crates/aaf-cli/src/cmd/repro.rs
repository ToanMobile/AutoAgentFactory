//! `aaf repro -- <program> [args...]`
//!
//! Manual operator pre-flight: confirms that a bug reproduces (command fails
//! every run) BEFORE creating a task. Intentionally **not** wired into
//! `aaf run` or `aaf gate`, preserving the `cd9c69a` security boundary.
//!
//! Exit codes:
//! - `0`: [`ReproOutcome::Confirmed`] (failed on every run)
//! - `2`: [`ReproOutcome::NotReproduced`], [`ReproOutcome::Flaky`],
//!   [`ReproOutcome::TimedOut`], [`ReproOutcome::Unrunnable`], or refusal when
//!   `AAF_RUN_ID` is set.

use std::time::Duration;

use aaf_engine::detector::repro::{DEFAULT_RUNS, ReproCommand, ReproOutcome, verify};
use serde_json::json;

use crate::ctx::Ctx;
use crate::output::{CmdError, CmdOutput, CmdResult};

const DEFAULT_REPRO_TIMEOUT: Duration = Duration::from_secs(60);

/// Arguments for `aaf repro -- <program> [args...]`.
#[derive(Debug, clap::Args)]
pub struct ReproArgs {
    /// Number of runs that must all fail to confirm the bug (default 2).
    #[arg(long, default_value_t = DEFAULT_RUNS)]
    pub runs: u32,

    /// Program and arguments to execute directly via argv (after `--`).
    #[arg(last = true, required = true)]
    pub argv: Vec<String>,
}

pub fn run(args: &ReproArgs, ctx: &Ctx) -> CmdResult {
    if let Some(refused) = refuse_if_agent(ctx, "aaf repro") {
        return Ok(refused);
    }
    let Some((program, extra_args)) = args.argv.split_first() else {
        return Err(CmdError::new("no program specified after --"));
    };
    let outcome = run_argv(program, extra_args, ctx, args.runs);
    Ok(outcome_output(&outcome, args.runs, false))
}

/// Runs manual repro pre-flight for `aaf board add --repro-cmd`.
/// Returns `None` if confirmed (caller should proceed to create the task),
/// or `Some(CmdOutput)` (exit 2) if refused or not confirmed.
pub(crate) fn preflight_for_board_add(
    program: &str,
    extra_args: &[String],
    runs: u32,
    ctx: &Ctx,
) -> Option<CmdOutput> {
    if let Some(refused) = refuse_if_agent(ctx, "aaf board add --repro-cmd") {
        return Some(refused);
    }
    let outcome = run_argv(program, extra_args, ctx, runs);
    match outcome {
        ReproOutcome::Confirmed => None,
        other => Some(outcome_output(&other, runs, true)),
    }
}

fn refuse_if_agent(ctx: &Ctx, what: &str) -> Option<CmdOutput> {
    ctx.run_id.as_ref().map(|run_id| {
        CmdOutput::policy(
            "refused",
            format!("{what} is a manual operator tool: refused inside agent run {run_id}"),
        )
    })
}

fn run_argv(program: &str, extra_args: &[String], ctx: &Ctx, runs: u32) -> ReproOutcome {
    let cmd = ReproCommand::argv(
        program,
        extra_args.to_vec(),
        ctx.cwd.clone(),
        DEFAULT_REPRO_TIMEOUT,
    );
    verify(&cmd, runs)
}

fn outcome_output(outcome: &ReproOutcome, runs: u32, for_board_add: bool) -> CmdOutput {
    let suffix = if for_board_add {
        "; task not created"
    } else {
        ""
    };
    match outcome {
        ReproOutcome::Confirmed => CmdOutput::ok(
            "confirmed",
            format!("repro confirmed: command failed on all {runs}/{runs} run(s)"),
        )
        .field("outcome", json!("confirmed"))
        .field("runs", json!(runs)),
        ReproOutcome::NotReproduced => CmdOutput::policy(
            "not_reproduced",
            format!("repro not confirmed: command passed on all {runs}/{runs} run(s){suffix}"),
        )
        .field("outcome", json!("not_reproduced"))
        .field("runs", json!(runs)),
        ReproOutcome::Flaky => CmdOutput::policy(
            "flaky",
            format!("repro not confirmed: flaky across {runs} run(s){suffix}"),
        )
        .field("outcome", json!("flaky"))
        .field("runs", json!(runs)),
        ReproOutcome::TimedOut => CmdOutput::policy(
            "timed_out",
            format!("repro not confirmed: command timed out{suffix}"),
        )
        .field("outcome", json!("timed_out"))
        .field("runs", json!(runs)),
        ReproOutcome::Unrunnable { reason } => {
            CmdOutput::policy("unrunnable", format!("repro unrunnable: {reason}{suffix}"))
                .field("outcome", json!("unrunnable"))
                .field("reason", json!(reason))
                .field("runs", json!(runs))
        }
    }
}
