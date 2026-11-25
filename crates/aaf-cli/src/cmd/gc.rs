//! `aaf gc`: clean orphaned batch worktrees, finished task worktrees/branches,
//! snapshot refs, stale fingerprint temp files, and expired stream/report files.

use std::fs;
use std::path::PathBuf;

use aaf_engine::gc::{DEFAULT_OLDER_THAN_DAYS, GcOptions, GcReport, run_gc};
use aaf_engine::git::main_work_tree;
use serde_json::Value;

use super::board::store::{err, refuse_inside_agent_run};
use crate::ctx::Ctx;
use crate::output::{CmdError, CmdOutput, CmdResult};

#[derive(Debug, clap::Args)]
pub struct GcArgs {
    /// Clean a specific repository path instead of the current directory.
    #[arg(long)]
    pub repo: Option<PathBuf>,
    /// Clean across all repositories recorded in the database.
    #[arg(long, conflicts_with = "repo")]
    pub all: bool,
    /// Preview what would be removed without modifying disk or git refs.
    #[arg(long)]
    pub dry_run: bool,
    /// Also clean worktrees, branches and snapshot refs of Cancelled tasks.
    #[arg(long)]
    pub include_cancelled: bool,
    /// Remove dirty worktrees for eligible tasks (skipped by default).
    #[arg(long)]
    pub force: bool,
    /// Delete expired `.jsonl`/`.html` stream and report files older than N days.
    #[arg(long, default_value_t = DEFAULT_OLDER_THAN_DAYS, value_name = "N")]
    pub older_than_days: u64,
}

fn resolve_repo(args: &GcArgs, ctx: &Ctx) -> Result<Option<PathBuf>, CmdError> {
    if args.all {
        return Ok(None);
    }
    if let Some(repo) = &args.repo {
        let candidate = if repo.is_absolute() {
            repo.clone()
        } else {
            ctx.cwd.join(repo)
        };
        if !candidate.exists() {
            return Err(CmdError::new(format!(
                "--repo {} does not exist",
                candidate.display()
            )));
        }
        let root = aaf_fp::repo_root(&candidate).unwrap_or(candidate);
        let main = main_work_tree(&root);
        let canonical = fs::canonicalize(&main).unwrap_or(main);
        return Ok(Some(canonical));
    }
    let root = aaf_fp::repo_root(&ctx.cwd)
        .map_err(|_| CmdError::new("not inside a git repository: pass --repo <path> or --all"))?;
    let main = main_work_tree(&root);
    let canonical = fs::canonicalize(&main).unwrap_or(main);
    Ok(Some(canonical))
}

fn format_report(report: &GcReport) -> String {
    let prefix = if report.dry_run { "gc (dry-run)" } else { "gc" };
    let mut lines = vec![format!(
        "{prefix}: {} repo(s) scanned, {} busy skipped, {} dirty skipped",
        report.repos_scanned.len(),
        report.skipped_busy.len(),
        report.skipped_dirty.len()
    )];
    lines.push(format!(
        "removed: {} orphan batch(es), {} worktree(s), {} branch(es), {} snapshot ref(s), \
         {} stale fp entr(y/ies), {} expired file(s) ({} proof file(s)); archived {} event(s); pruned {} repo(s)",
        report.removed_orphan_batches.len(),
        report.removed_worktrees.len(),
        report.removed_branches.len(),
        report.removed_snapshot_refs,
        report.removed_stale_fp,
        report.removed_files.len(),
        report.proofs_removed,
        report.events_archived,
        report.pruned_repos
    ));
    if !report.warnings.is_empty() {
        lines.push(format!("warnings: {}", report.warnings.join("; ")));
    }
    lines.join("\n")
}

pub fn run(args: &GcArgs, ctx: &Ctx) -> CmdResult {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf gc") {
        return Ok(refused);
    }
    let repo_root = resolve_repo(args, ctx)?;
    let opts = GcOptions {
        repo_root,
        all: args.all,
        dry_run: args.dry_run,
        include_cancelled: args.include_cancelled,
        force: args.force,
        older_than_days: args.older_than_days,
        cwd: Some(ctx.cwd.clone()),
    };
    let report = run_gc(&ctx.data_dir, &ctx.worktrees_dir, &opts).map_err(|e| err("gc", e))?;
    let mut out = CmdOutput::ok("ok", format_report(&report));
    let Value::Object(fields) = serde_json::to_value(&report).map_err(|e| err("gc report", e))?
    else {
        return Err(CmdError::new("gc report is not an object"));
    };
    out.fields.extend(fields);
    Ok(out)
}
