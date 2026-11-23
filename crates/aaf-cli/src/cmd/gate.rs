//! `aaf gate [--scoped|--full|--release]`: run the gate, report the receipt.
//!
//! Exit 0 for `CLEAR`, 2 for `BLOCKED` or `UNVERIFIED`, 1 when the gate could
//! not run at all (bad config, git failure).

use std::path::Path;
use std::time::{Duration, Instant};

use aaf_domain::gate::{GateMode, GateStatus, Receipt, Severity};
use aaf_engine::config::EffectiveConfig;
use aaf_engine::gate::{GateError, GateRun};
use aaf_engine::throttle::{Busy, Progress, SystemProbe, idle};
use serde_json::json;

use crate::ctx::{Ctx, SystemClock};
use crate::output::{CmdError, CmdOutput, CmdResult, notice};

#[derive(Debug, clap::Args)]
#[group(multiple = false)]
struct ModeArgs {
    /// Lint and the scoped tests (default).
    #[arg(long)]
    scoped: bool,
    /// Lint, build and the whole suite.
    #[arg(long)]
    full: bool,
    /// As --full, plus the release-only store compliance rules.
    #[arg(long)]
    release: bool,
}

#[derive(Debug, clap::Args)]
pub struct GateArgs {
    #[command(flatten)]
    mode: ModeArgs,
    /// Only run the gate when the machine is idle (no active heavy slots,
    /// no active external Gradle/Unity/xcodebuild processes, normal memory
    /// pressure, calm CPU load). Exits 2 (`BUSY`) if still busy after waiting.
    #[arg(long)]
    when_idle: bool,
    /// Seconds to wait for the machine to become idle when `--when-idle` is set.
    #[arg(long, default_value_t = 0)]
    idle_wait_secs: u64,
}

impl GateArgs {
    fn mode(&self) -> GateMode {
        if self.mode.release {
            GateMode::Release
        } else if self.mode.full {
            GateMode::Full
        } else {
            GateMode::Scoped
        }
    }
}

/// One short line per report of the throttle (PLAN C.11: waiting for the
/// machine is shown, it is not an error).
pub(crate) fn progress_line(event: &Progress<'_>) -> String {
    const MB: u64 = 1024 * 1024;
    match event {
        Progress::WaitingForSlot {
            label,
            reason,
            waited_secs,
        } => {
            let why = match reason {
                Busy::SlotsTaken => "every heavy slot is taken".to_string(),
                Busy::Draining => {
                    "queue is draining for an unmeasured or exclusive command".to_string()
                }
                Busy::RunsAlone => {
                    "it runs alone: never measured, or a build tool that works through daemons"
                        .to_string()
                }
                Busy::NotEnoughMemory { free, needed } => match free {
                    Some(free) => format!("needs {} MB free, {} MB are", needed / MB, free / MB),
                    None => format!("needs {} MB free, free memory unreadable", needed / MB),
                },
                Busy::Orphaned { pgid } => format!(
                    "process group {pgid} is recorded as the command of a killed aaf and \
                     still has processes; the slot is free when it ends. Look before ending \
                     it (`ps -o pid,lstart,command -g {pgid}`): the record names a group id, \
                     and an id can have gone to another program since; if it is the old \
                     build, `kill -TERM -- -{pgid}` ends it"
                ),
            };
            format!("{label}: waiting for a heavy slot ({why}), {waited_secs}s")
        }
        Progress::WaitingForMemory {
            label,
            pressure,
            waited_secs,
        } => format!("{label}: waiting for memory pressure to ease ({pressure:?}), {waited_secs}s"),
        Progress::WaitingForLoad {
            label,
            load_percent,
            max_load_percent,
            cpu_busy_percent,
            max_cpu_percent,
            waited_secs,
            gives_up_after_secs,
        } => match (cpu_busy_percent, max_cpu_percent) {
            (Some(busy), Some(max_cpu)) => format!(
                "{label}: waiting for CPU busy {busy}% to drop below {max_cpu}% \
                 (loadavg {load_percent}%), {waited_secs}s (runs anyway after {gives_up_after_secs}s)"
            ),
            _ => format!(
                "{label}: waiting for CPU load {load_percent}% to drop to {max_load_percent}%, \
                 {waited_secs}s (runs anyway after {gives_up_after_secs}s)"
            ),
        },
        Progress::WaitingForMergeLock {
            pr_number,
            waited_secs,
            timeout_secs,
        } => format!(
            "PR #{pr_number}: waiting for another merge in this repo to finish, \
             {waited_secs}s (timeout {timeout_secs}s)"
        ),
        Progress::Paused { label, slot } => {
            format!("{label}: paused under critical memory pressure (slot {slot})")
        }
        Progress::Resumed { label, slot } => format!("{label}: resumed (slot {slot})"),
        Progress::ProbeUnreadable { what } => {
            format!("{what} could not be read; its fallback applies")
        }
        Progress::Unkilled { label, pgid } => format!(
            "{label}: process group {pgid} still had members after repeated SIGKILL; \
             they may still be running (`ps -o pid,command -g {pgid}`)"
        ),
    }
}

/// The one call into the engine's gate, under the machine limits of tier 1.
fn call_gate(
    ctx: &Ctx,
    repo_root: &Path,
    cfg: &EffectiveConfig,
    mode: GateMode,
) -> Result<GateRun, GateError> {
    let progress = |event: &Progress<'_>| notice("gate", &progress_line(event));
    aaf_engine::gate::run_gate_observed_reporting(
        &ctx.data_dir,
        repo_root,
        &cfg.repo,
        mode,
        &SystemClock,
        &cfg.global.machine,
        &progress,
    )
}

fn count<T>(items: &[T], severity_of: impl Fn(&T) -> Severity, wanted: Severity) -> usize {
    items
        .iter()
        .filter(|item| severity_of(item) == wanted)
        .count()
}

fn location(file: &str, line: Option<u32>) -> String {
    match line {
        Some(line) => format!("{file}:{line}"),
        None => file.to_string(),
    }
}

fn human(receipt: &Receipt, status: &str, receipt_path: &Path, warnings: &[String]) -> String {
    let mut text = format!("{status} ({})", mode_word(receipt.mode));
    for reason in &receipt.reasons {
        text.push_str(&format!("\n  reason: {reason}"));
    }
    for f in &receipt.static_findings {
        text.push_str(&format!(
            "\n  {} {:?} {}: {} (fix: {})",
            f.rule,
            f.severity,
            location(&f.file, f.line),
            f.message,
            f.fix
        ));
    }
    if receipt.suppressions > 0 {
        text.push_str(&format!("\n  suppressions: {}", receipt.suppressions));
    }
    for f in &receipt.store_compliance.findings {
        text.push_str(&format!(
            "\n  {} {:?} {}: {} (fix: {})",
            f.rule,
            f.severity,
            location(&f.file, f.line),
            f.message,
            f.fix
        ));
    }
    for change in &receipt.head_lock.locked_changes {
        text.push_str(&format!(
            "\n  locked {:?} {} ({:?})",
            change.change, change.path, receipt.head_lock.status
        ));
    }
    for command in &receipt.commands {
        let outcome = match (command.timed_out, command.exit_code) {
            (true, _) => "timed out".to_string(),
            (false, Some(code)) => format!("exit {code}"),
            (false, None) => "killed by a signal".to_string(),
        };
        text.push_str(&format!(
            "\n  {}: {outcome} in {} ms: {}",
            command.name, command.duration_ms, command.command
        ));
    }
    for warning in warnings {
        text.push_str(&format!("\n  config warning: {warning}"));
    }
    text.push_str(&format!(
        "\n  tree: {}\n  receipt: {}",
        receipt.tree_fp,
        receipt_path.display()
    ));
    text
}

fn mode_word(mode: GateMode) -> &'static str {
    match mode {
        GateMode::Scoped => "scoped",
        GateMode::Full => "full",
        GateMode::Release => "release",
    }
}

pub fn run(args: &GateArgs, ctx: &Ctx) -> CmdResult {
    let repo_root = ctx.repo_root()?;
    let cfg = ctx.load_config(&repo_root)?;
    if args.when_idle {
        let probe = SystemProbe::detect();
        let deadline = Instant::now() + Duration::from_secs(args.idle_wait_secs);
        let mut last_blockers = idle::probe_idle(&ctx.data_dir, &cfg.global.machine, &probe);
        while !last_blockers.is_empty() && Instant::now() < deadline {
            notice(
                "gate",
                &format!("waiting for idle machine ({})", last_blockers.join("; ")),
            );
            let remaining = deadline.saturating_duration_since(Instant::now());
            std::thread::sleep(remaining.min(Duration::from_secs(2)));
            last_blockers = idle::probe_idle(&ctx.data_dir, &cfg.global.machine, &probe);
        }
        if !last_blockers.is_empty() {
            let mut text = format!("BUSY ({}, deferred by --when-idle)", mode_word(args.mode()));
            for b in &last_blockers {
                text.push_str(&format!("\n  blocker: {b}"));
            }
            return Ok(CmdOutput::policy("BUSY", text)
                .field("mode", json!(args.mode()))
                .field("deferred", json!(true))
                .field("idle_blockers", json!(last_blockers)));
        }
    }
    let GateRun { receipt, reused } =
        call_gate(ctx, &repo_root, &cfg, args.mode()).map_err(|e| {
            let mut message = format!("gate: {e}");
            if let Some(run_id) = &ctx.run_id {
                // The agent of an `aaf run`: in its sandbox the data
                // directory (signing key, receipts, slot files) is closed,
                // and a bare I/O error says nothing about why.
                message.push_str(&format!(
                    ". This is agent run {run_id}: the gate is run by the factory outside the \
                     sandbox when the run ends, so do not run `aaf gate` here. Finish the \
                     change and end the run; a heavy build or test goes through \
                     `aaf slot -- <command>`"
                ));
            }
            CmdError::new(message)
        })?;

    let status = match receipt.status {
        GateStatus::Clear => "CLEAR",
        GateStatus::Blocked => "BLOCKED",
        GateStatus::Unverified => "UNVERIFIED",
    };
    // The receipt carries the canonical root the engine filed it under.
    let receipt_path = aaf_fp::receipt_path(&ctx.data_dir, Path::new(&receipt.repo_root));
    let mut text = human(&receipt, status, &receipt_path, &cfg.warnings);
    if reused {
        text.push_str(
            "\n  reused receipt: same tree, base and gate configuration as a stored Clear \
             receipt; nothing was run now",
        );
    }
    let out = if receipt.status == GateStatus::Clear {
        CmdOutput::ok(status, text)
    } else {
        CmdOutput::policy(status, text)
    };
    let statics = &receipt.static_findings;
    // The gate files what it only warns about among the receipt's reasons
    // (`warning: work tree: …`). They decide nothing, so they are easy to
    // miss there: shown on stderr as well, and a list of their own for a
    // script. `reasons` itself stays as the receipt has it.
    let warnings: Vec<&String> = receipt
        .reasons
        .iter()
        .filter(|reason| reason.starts_with("warning: "))
        .collect();
    for warning in &warnings {
        notice("gate", warning);
    }
    let docs_fast_path = receipt
        .reasons
        .iter()
        .any(|r| r.starts_with(aaf_engine::gate::docs_fast::DOCS_FAST_PATH_NOTE));
    Ok(out
        .field("mode", json!(receipt.mode))
        .field("repo_root", json!(receipt.repo_root))
        .field("docs_fast_path", json!(docs_fast_path))
        .field("reasons", json!(receipt.reasons))
        .field("warnings", json!(warnings))
        .field("tree_fp", json!(receipt.tree_fp))
        .field("head_sha", json!(receipt.head_sha))
        .field("base_sha", json!(receipt.base_sha))
        .field("receipt_path", json!(receipt_path))
        // The commands listed below ran when the receipt was made, not now.
        .field("reused_receipt", json!(reused))
        .field(
            "findings",
            json!({
                "static_critical": count(statics, |f| f.severity, Severity::Critical),
                "static_warn": count(statics, |f| f.severity, Severity::Warn),
                "suppressions": receipt.suppressions,
                "store_critical": receipt.store_compliance.critical,
                "store_warn": receipt.store_compliance.warnings,
                "store_unresolved": receipt.store_compliance.unresolved.len(),
            }),
        )
        .field("static_findings", json!(statics))
        .field("head_lock", json!(receipt.head_lock))
        .field("store_compliance", json!(receipt.store_compliance))
        .field("commands", json!(receipt.commands))
        .field("config_warnings", json!(cfg.warnings)))
}
