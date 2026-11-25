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
#[command(
    name = "aaf",
    version = env!("AAF_BUILD_VERSION"),
    about = "AutoAgentFactory"
)]
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
    /// Verify a PR on latest base under the merge lock and merge it via GitHub.
    Merge(cmd::merge::MergeArgs),
    /// Check the installation on this machine and in this repo.
    Doctor(cmd::doctor::DoctorArgs),
    /// List accounts, pick the next one, set or clear a cooldown.
    Accounts(cmd::accounts::AccountsArgs),
    /// Run the gate and write a signed receipt.
    Gate(cmd::gate::GateArgs),
    /// Clean orphaned worktrees, finished task refs and expired stream/report files.
    Gc(cmd::gc::GcArgs),
    /// Build a read-only acceptance handover report (Markdown, HTML, or JSON).
    Handover(cmd::handover::HandoverArgs),
    /// Sign an approval for the current head-lock fingerprint.
    Approve(cmd::approve::ApproveArgs),
    /// Add, list, show, unblock tasks; unfreeze the factory.
    Board(cmd::board::BoardArgs),
    /// Synchronous interactive TTY cockpit for inspecting and steering tasks.
    Cockpit(cmd::cockpit::CockpitArgs),
    /// Run ready tasks through agent, gate and merge point.
    Run(cmd::run::RunArgs),
    /// Run a command through the machine-wide queue of heavy commands.
    Slot(cmd::slot::SlotArgs),
    /// Show read-only cost, token, task/run, account and throttle overview.
    Status(cmd::status::StatusArgs),
    /// Build a strictly read-only planning prompt.
    Plan(cmd::plan::PlanArgs),
    /// Record, verify, or inspect signed device-verification proofs.
    Proof(cmd::proof::ProofArgs),
    /// List or sync bundled domain skills into `.claude/skills`.
    Skills(cmd::skills::SkillsArgs),
    /// List, inspect, scaffold, validate, run, and approve declarative SOP workflows.
    Sop(cmd::sop::SopArgs),
    /// Inspect installed coding-agent CLIs, fallback routing, role profiles, and `.mcp.json` hygiene.
    Executors(cmd::executors::ExecutorsArgs),
    /// Run the outbound Telegram remote companion (status, handover, two-step approvals).
    Telegram(cmd::telegram::TelegramArgs),
    /// Start the authenticated local/LAN Web & Mobile Cockpit (HTTP + SSE + QR).
    Ui(cmd::ui::UiArgs),
    /// Self-update `aaf` and `aaf-hook` (auto-builds from source inside the AAF repo, or downloads SHA-256 verified release; human-only).
    #[command(visible_alias = "update")]
    Upgrade(cmd::upgrade::UpgradeArgs),
    /// Generate ready-to-eval shell completion scripts (bash, zsh, fish, powershell).
    Completions(cmd::completions::CompletionsArgs),
    /// Live read-only terminal dashboard for tasks, heavy slots, accounts and receipt.
    Watch(cmd::watch::WatchArgs),
    /// Push "needs you" moments (frozen, blocked, ready for review) to the webhook / Telegram.
    Notify(cmd::notify::NotifyArgs),
    /// Manual operator pre-flight: confirm a bug reproduces before creating a task.
    Repro(cmd::repro::ReproArgs),
    /// Period report: tasks done, cost per executor, time per lifecycle stage.
    Report(cmd::report::ReportArgs),
    /// One-step setup: detect agents, register hooks, sync skills, run doctor. Safe to re-run.
    Setup(cmd::setup::SetupArgs),
}

impl Command {
    fn name(&self) -> &'static str {
        match self {
            Command::Init(_) => "init",
            Command::Merge(_) => "merge",
            Command::Doctor(_) => "doctor",
            Command::Accounts(_) => "accounts",
            Command::Gate(_) => "gate",
            Command::Gc(_) => "gc",
            Command::Handover(_) => "handover",
            Command::Approve(_) => "approve",
            Command::Board(_) => "board",
            Command::Cockpit(_) => "cockpit",
            Command::Run(_) => "run",
            Command::Slot(_) => "slot",
            Command::Status(_) => "status",
            Command::Plan(_) => "plan",
            Command::Proof(_) => "proof",
            Command::Skills(_) => "skills",
            Command::Sop(_) => "sop",
            Command::Executors(_) => "executors",
            Command::Telegram(_) => "telegram",
            Command::Ui(_) => "ui",
            Command::Upgrade(_) => "upgrade",
            Command::Completions(_) => "completions",
            Command::Watch(_) => "watch",
            Command::Notify(_) => "notify",
            Command::Repro(_) => "repro",
            Command::Report(_) => "report",
            Command::Setup(_) => "setup",
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
        Command::Merge(args) => cmd::merge::run(args, &ctx),
        Command::Doctor(args) => cmd::doctor::run(args, &ctx),
        Command::Accounts(args) => cmd::accounts::run(args, &ctx),
        Command::Gate(args) => cmd::gate::run(args, &ctx),
        Command::Gc(args) => cmd::gc::run(args, &ctx),
        Command::Handover(args) => cmd::handover::run(args, &ctx),
        Command::Approve(args) => cmd::approve::run(args, &ctx),
        Command::Board(args) => cmd::board::run(args, &ctx),
        Command::Cockpit(args) => cmd::cockpit::run(args, &ctx),
        Command::Run(args) => cmd::run::run(args, &ctx),
        Command::Slot(args) => cmd::slot::run(args, &ctx),
        Command::Status(args) => cmd::status::run(args, &ctx),
        Command::Plan(args) => cmd::plan::run(args, &ctx),
        Command::Proof(args) => cmd::proof::run(args, &ctx),
        Command::Skills(args) => cmd::skills::run(args, &ctx),
        Command::Sop(args) => cmd::sop::run(args, &ctx),
        Command::Executors(args) => cmd::executors::run(args, &ctx),
        Command::Telegram(args) => cmd::telegram::run(args, &ctx),
        Command::Ui(args) => cmd::ui::run(args, &ctx),
        Command::Upgrade(args) => cmd::upgrade::run(args, &ctx),
        Command::Completions(args) => cmd::completions::run(args, &ctx),
        Command::Watch(args) => cmd::watch::run(args, &ctx, cli.json),
        Command::Notify(args) => cmd::notify::run(args, &ctx),
        Command::Repro(args) => cmd::repro::run(args, &ctx),
        Command::Report(args) => cmd::report::run(args, &ctx),
        Command::Setup(args) => cmd::setup::run(args, &ctx),
    });
    let millis = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    ExitCode::from(output::emit(cli.json, name, &result, millis))
}
