//! `aaf merge <PR> --expect-head <sha40>`: locked Pull Request merge gate.
//!
//! Exit 0 for `MERGED`, `DRY_RUN_CLEAR`, or `ALREADY_MERGED`; exit 2 for
//! policy blocks (head mismatch, conflict, CI failure, unapproved head-lock,
//! gate blocked); exit 1 for operational/git/gh errors.

use std::path::PathBuf;
use std::time::Duration;

use aaf_domain::gate::GateMode;
use aaf_engine::git::{MergeStrategy, PrMergeOutcome, PrMergeRequest, SystemGh, merge_pr};
use aaf_engine::throttle::Progress;
use clap::ValueEnum;
use serde_json::json;

use crate::cmd::gate::progress_line;
use crate::ctx::{Ctx, SystemClock};
use crate::output::{CmdError, CmdOutput, CmdResult, notice};

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ModeArg {
    Scoped,
    Full,
    Release,
}

impl From<ModeArg> for GateMode {
    fn from(m: ModeArg) -> Self {
        match m {
            ModeArg::Scoped => GateMode::Scoped,
            ModeArg::Full => GateMode::Full,
            ModeArg::Release => GateMode::Release,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum StrategyArg {
    Squash,
    Merge,
    Rebase,
}

impl From<StrategyArg> for MergeStrategy {
    fn from(s: StrategyArg) -> Self {
        match s {
            StrategyArg::Squash => MergeStrategy::Squash,
            StrategyArg::Merge => MergeStrategy::Merge,
            StrategyArg::Rebase => MergeStrategy::Rebase,
        }
    }
}

#[derive(Debug, clap::Args)]
pub struct MergeArgs {
    /// Pull request number to verify and merge.
    pub pr_number: u64,
    /// Exact 40-character commit SHA expected at the head of the PR.
    #[arg(long)]
    pub expect_head: String,
    /// Gate mode to run on the merged tree (default: scoped).
    #[arg(long, value_enum, default_value_t = ModeArg::Scoped)]
    mode: ModeArg,
    /// GitHub merge strategy (default: squash).
    #[arg(long, value_enum, default_value_t = StrategyArg::Squash)]
    strategy: StrategyArg,
    /// Git remote name (default: origin).
    #[arg(long, default_value = "origin")]
    remote: String,
    /// Optional commit subject passed to `gh pr merge --subject`.
    #[arg(long)]
    subject: Option<String>,
    /// Optional commit body file passed to `gh pr merge --body-file`.
    #[arg(long)]
    body_file: Option<PathBuf>,
    /// Seconds to wait for pending GitHub PR checks before failing closed.
    #[arg(long, default_value_t = 0)]
    ci_wait: u64,
    /// Seconds to wait for the per-repo merge lock.
    #[arg(long, default_value_t = 300)]
    lock_timeout: u64,
    /// Verify the merged tree under the lock without merging on GitHub.
    #[arg(long)]
    dry_run: bool,
}

pub fn run(args: &MergeArgs, ctx: &Ctx) -> CmdResult {
    let repo_root = ctx.repo_root()?;
    let cfg = ctx.load_config(&repo_root)?;
    let req = PrMergeRequest {
        repo_root,
        data_dir: ctx.data_dir.clone(),
        pr_number: args.pr_number,
        expect_head: args.expect_head.clone(),
        remote: args.remote.clone(),
        mode: args.mode.into(),
        strategy: args.strategy.into(),
        subject: args.subject.clone(),
        body_file: args.body_file.clone(),
        ci_wait_secs: args.ci_wait,
        lock_timeout: Duration::from_secs(args.lock_timeout),
        dry_run: args.dry_run,
        machine: cfg.global.machine.clone(),
    };
    let progress = |event: &Progress<'_>| notice("merge", &progress_line(event));
    let outcome = merge_pr(&req, &SystemGh, &SystemClock, &progress)
        .map_err(|e| CmdError::new(format!("merge: {e}")))?;

    match outcome {
        PrMergeOutcome::Merged {
            pr_number,
            head_sha,
            base_sha,
            receipt,
            local_sync_note,
        } => {
            let text = format!(
                "MERGED PR #{pr_number}\n  head: {head_sha}\n  base: {base_sha}\n  tree: {}\n  local: {local_sync_note}",
                receipt.tree_fp
            );
            Ok(CmdOutput::ok("MERGED", text)
                .field("pr_number", json!(pr_number))
                .field("head_sha", json!(head_sha))
                .field("base_sha", json!(base_sha))
                .field("local_sync_note", json!(local_sync_note))
                .field("receipt", json!(receipt)))
        }
        PrMergeOutcome::DryRunClear {
            pr_number,
            head_sha,
            base_sha,
            receipt,
        } => {
            let text = format!(
                "DRY_RUN_CLEAR PR #{pr_number}\n  head: {head_sha}\n  base: {base_sha}\n  tree: {}",
                receipt.tree_fp
            );
            Ok(CmdOutput::ok("DRY_RUN_CLEAR", text)
                .field("pr_number", json!(pr_number))
                .field("head_sha", json!(head_sha))
                .field("base_sha", json!(base_sha))
                .field("dry_run", json!(true))
                .field("receipt", json!(receipt)))
        }
        PrMergeOutcome::AlreadyMerged { pr_number, url } => {
            let text = format!("ALREADY_MERGED PR #{pr_number} ({url})");
            Ok(CmdOutput::ok("ALREADY_MERGED", text)
                .field("pr_number", json!(pr_number))
                .field("url", json!(url)))
        }
        PrMergeOutcome::Blocked {
            code,
            reason,
            receipt,
        } => {
            let text = format!("BLOCKED ({code}): {reason}");
            Ok(CmdOutput::policy("BLOCKED", text)
                .field("code", json!(code))
                .field("reason", json!(reason))
                .field("receipt", json!(receipt)))
        }
    }
}
