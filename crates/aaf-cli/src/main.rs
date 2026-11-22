//! `aaf`: command line entry point (docs/PLAN.md section C.10).
//!
//! This file only parses arguments and dispatches. Each subcommand lives in
//! `cmd/<name>.rs` and returns an [`output::CmdResult`]; printing and the exit
//! code are decided in one place, [`output::emit`].

mod cmd;
mod ctx;
mod output;

use std::process::ExitCode;
use std::time::Instant;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "aaf", version, about = "AutoAgentFactory")]
struct Cli {
    /// Print exactly one JSON object on stdout.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Detect the stack, write `.autofactory/config.toml`, register the hook.
    Init(cmd::init::InitArgs),
    /// Check the installation on this machine and in this repo.
    Doctor(cmd::doctor::DoctorArgs),
    /// List accounts, pick the next one, set or clear a cooldown.
    Accounts(cmd::accounts::AccountsArgs),
    /// Run the gate and write a signed receipt.
    Gate(cmd::gate::GateArgs),
    /// Sign an approval for the current head-lock fingerprint.
    Approve(cmd::approve::ApproveArgs),
    /// Add, list, show, unblock tasks; unfreeze the factory.
    Board(cmd::board::BoardArgs),
    /// Run ready tasks through agent, gate and merge point.
    Run(cmd::run::RunArgs),
    /// Run a command through the machine-wide queue of heavy commands.
    Slot(cmd::slot::SlotArgs),
}

impl Command {
    fn name(&self) -> &'static str {
        match self {
            Command::Init(_) => "init",
            Command::Doctor(_) => "doctor",
            Command::Accounts(_) => "accounts",
            Command::Gate(_) => "gate",
            Command::Approve(_) => "approve",
            Command::Board(_) => "board",
            Command::Run(_) => "run",
            Command::Slot(_) => "slot",
        }
    }
}

/// A command line clap rejected, or a `--help`/`--version` request. Without
/// `--json` clap prints and exits as usual; with it the one-object contract
/// holds here too: exit 2 for a usage error, 0 for help.
fn usage(err: &clap::Error, started: Instant) -> ExitCode {
    if !ctx::json_requested() {
        err.exit();
    }
    let message = err.render().to_string();
    let out = if err.exit_code() == 0 {
        output::CmdOutput::ok("help", "").field("text", serde_json::json!(message))
    } else {
        output::CmdOutput::policy("usage_error", "").field("error", serde_json::json!(message))
    };
    let millis = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    ExitCode::from(output::emit(true, "aaf", &Ok(out), millis))
}

fn main() -> ExitCode {
    let started = Instant::now();
    // Before anything runs: every git and gh call of this process uses the
    // path looked up now, not what a PATH directory holds later.
    aaf_fp::resolve_programs();
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => return usage(&err, started),
    };
    let name = cli.command.name();
    let result = ctx::Ctx::from_process_env().and_then(|ctx| match &cli.command {
        Command::Init(args) => cmd::init::run(args, &ctx),
        Command::Doctor(args) => cmd::doctor::run(args, &ctx),
        Command::Accounts(args) => cmd::accounts::run(args, &ctx),
        Command::Gate(args) => cmd::gate::run(args, &ctx),
        Command::Approve(args) => cmd::approve::run(args, &ctx),
        Command::Board(args) => cmd::board::run(args, &ctx),
        Command::Run(args) => cmd::run::run(args, &ctx),
        Command::Slot(args) => cmd::slot::run(args, &ctx),
    });
    let millis = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    ExitCode::from(output::emit(cli.json, name, &result, millis))
}
