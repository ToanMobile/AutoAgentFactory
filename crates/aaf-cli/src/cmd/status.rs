//! `aaf status`: read-only cost, token, task/run, account and throttle overview.

use std::collections::BTreeMap;

use aaf_domain::gate::LockStatus;
use aaf_domain::ports::Clock;
use aaf_engine::factory::adapters::SystemClock;
use aaf_engine::gate::{GIT_TIMEOUT, base, head_lock};
use aaf_engine::telemetry::{
    CostRates, RunCounts, StatusQuery, TaskCounts, TelemetrySummary, collect_status_from_db,
    inspect_throttle,
};
use serde_json::json;

use super::board::store::{err, open_db_read_only, repo_root};
use crate::ctx::Ctx;
use crate::output::{CmdError, CmdOutput, CmdResult};

#[derive(Debug, clap::Args)]
pub struct StatusArgs {
    /// Summarize across every repository instead of scoping to the current repo.
    #[arg(long)]
    pub all: bool,
    /// Inspect read-only head-lock approval state for the current repo/worktree.
    #[arg(long)]
    pub approval: bool,
}

fn run_approval_status(ctx: &Ctx) -> CmdResult {
    let root = ctx.repo_root()?;
    let cfg = ctx.load_config(&root)?;
    let resolved = base::resolve(&root, &cfg.repo.gate.base_ref, GIT_TIMEOUT)
        .map_err(|e| CmdError::new(format!("status --approval: {e}")))?;
    let report = head_lock::evaluate(&ctx.data_dir, &root, &cfg.repo, &resolved.diff_target);
    let mut merge_cfg = cfg.repo.clone();
    merge_cfg.cross_audit.require_different_provider = true;
    let merge_report = head_lock::evaluate(&ctx.data_dir, &root, &merge_cfg, &resolved.diff_target);

    let status_word = match report.status {
        LockStatus::Clear => "clear",
        LockStatus::Approved => "approved",
        LockStatus::Blocked => "missing",
    };
    let approved = report.status == LockStatus::Approved;
    let independent_approved = merge_report.status == LockStatus::Approved;
    let approval_rec = merge_report.approval.as_ref().or(report.approval.as_ref());
    let reviewer = approval_rec.map(|a| a.reviewer.as_str());
    let same_account = approval_rec.map(|a| a.same_account);
    let locked_files: Vec<String> = report
        .locked_changes
        .iter()
        .map(|c| c.path.clone())
        .collect();

    let mut lines = vec![
        format!("scope: {}", root.display()),
        format!("base: {}", resolved.diff_target),
    ];
    match report.status {
        LockStatus::Clear => {
            lines.push("approval: clear (no locked files changed)".to_string());
        }
        LockStatus::Approved => {
            lines.push(format!(
                "approval: approved (independent={}, reviewer={}, lock_fp={}, locked_files={})",
                independent_approved,
                reviewer.unwrap_or("unknown"),
                report.lock_fp,
                locked_files.len()
            ));
        }
        LockStatus::Blocked => {
            lines.push(format!(
                "approval: missing (lock_fp={}, locked_files={})",
                report.lock_fp,
                locked_files.len()
            ));
        }
    }
    for file in &locked_files {
        lines.push(format!("  locked: {file}"));
    }

    let approval_json = json!({
        "status": status_word,
        "base": resolved.diff_target,
        "lock_fp": report.lock_fp,
        "locked_files": locked_files,
        "approved": approved,
        "independent_approved": independent_approved,
        "reviewer": reviewer,
        "same_account": same_account,
    });

    Ok(CmdOutput::ok("ok", lines.join("\n"))
        .field("scope", json!(root.to_string_lossy().into_owned()))
        .field("approval", approval_json))
}

pub fn run(args: &StatusArgs, ctx: &Ctx) -> CmdResult {
    if args.approval {
        return run_approval_status(ctx);
    }
    let db = open_db_read_only(ctx)?;
    let repo = if args.all {
        None
    } else {
        repo_root(ctx)
            .ok()
            .map(|path| path.to_string_lossy().into_owned())
    };
    let scope = if args.all {
        "all".to_string()
    } else if let Some(root) = &repo {
        root.clone()
    } else {
        "all (outside git repo)".to_string()
    };

    let summary = match db {
        Some(db) => collect_status_from_db(
            &db,
            &StatusQuery {
                repo_root: repo.as_deref(),
                data_dir: Some(&ctx.data_dir),
                now: SystemClock.now(),
                rates: CostRates::default(),
            },
        )
        .map_err(|e| CmdError::new(e.to_string()))?,
        None => TelemetrySummary {
            frozen: false,
            consecutive_root_fails: 0,
            repo_root: repo.clone(),
            counts: TaskCounts::default(),
            blocked_reasons: BTreeMap::new(),
            runs: RunCounts::default(),
            tokens_in: 0,
            tokens_out: 0,
            estimated_cost_usd: 0.0,
            accounts: Vec::new(),
            throttle: inspect_throttle(&ctx.data_dir),
        },
    };

    let blocked_text = if summary.blocked_reasons.is_empty() {
        "none".to_string()
    } else {
        summary
            .blocked_reasons
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(", ")
    };

    let lines = [
        format!("scope: {scope}"),
        format!(
            "tasks: {} total (backlog={}, ready={}, running={}, gating={}, review={}, blocked={}, done={}, cancelled={})",
            summary.counts.total,
            summary.counts.backlog,
            summary.counts.ready,
            summary.counts.running,
            summary.counts.gating,
            summary.counts.review,
            summary.counts.blocked,
            summary.counts.done,
            summary.counts.cancelled
        ),
        format!("blocked_reasons: {blocked_text}"),
        format!(
            "runs: {} total (running={}, succeeded={}, failed={}, timed_out={}, limit_hit={}, killed={})",
            summary.runs.total,
            summary.runs.running,
            summary.runs.succeeded,
            summary.runs.failed,
            summary.runs.timed_out,
            summary.runs.limit_hit,
            summary.runs.killed
        ),
        format!(
            "tokens: {} in + {} out (~${:.4})",
            summary.tokens_in, summary.tokens_out, summary.estimated_cost_usd
        ),
        format!(
            "factory: frozen={}, consecutive_root_fails={}, accounts={}, active_slots={} (lock_files={}), draining={}",
            summary.frozen,
            summary.consecutive_root_fails,
            summary.accounts.len(),
            summary.throttle.throttle_slots,
            summary.throttle.slot_lock_files,
            summary.throttle.drain_lock_present
        ),
    ];

    let throttle_json = json!({
        "throttle_dir_exists": summary.throttle.throttle_dir_exists,
        "drain_lock_present": summary.throttle.drain_lock_present,
        "draining": summary.throttle.drain_lock_present,
        "throttle_slots": summary.throttle.throttle_slots,
        "active_slots": summary.throttle.throttle_slots,
        "slot_lock_files": summary.throttle.slot_lock_files,
        "guest_record_files": summary.throttle.guest_record_files,
    });

    Ok(CmdOutput::ok("ok", lines.join("\n"))
        .field("scope", json!(scope))
        .field("frozen", json!(summary.frozen))
        .field(
            "consecutive_root_fails",
            json!(summary.consecutive_root_fails),
        )
        .field("repo_root", json!(summary.repo_root))
        .field(
            "tasks",
            serde_json::to_value(&summary.counts).map_err(|e| err("tasks", e))?,
        )
        .field(
            "blocked_reasons",
            serde_json::to_value(&summary.blocked_reasons)
                .map_err(|e| err("blocked_reasons", e))?,
        )
        .field(
            "runs",
            serde_json::to_value(&summary.runs).map_err(|e| err("runs", e))?,
        )
        .field("tokens_in", json!(summary.tokens_in))
        .field("tokens_out", json!(summary.tokens_out))
        .field("estimated_cost_usd", json!(summary.estimated_cost_usd))
        .field(
            "accounts",
            serde_json::to_value(&summary.accounts).map_err(|e| err("accounts", e))?,
        )
        .field("throttle", throttle_json))
}
