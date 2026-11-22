//! `aaf board import-issues [--label <name>] [--dry-run]` and
//! `aaf board sync-github [--dry-run]`: fail-closed GitHub issue intake and
//! Super-Board column synchronization.

use std::io;
use std::path::Path;
use std::time::Duration;

use aaf_domain::ports::Clock;
use aaf_engine::factory::Store;
use aaf_engine::factory::adapters::SystemClock;
use aaf_engine::github_board::{
    SyncOutcome, has_trusted_login, import_issues_dry_run, import_issues_with_options, sync_board,
};
use aaf_engine::process::{CommandOutput, CommandRunner, SystemRunner};
use serde_json::json;

use super::port;
use super::store::{err, open_db, open_db_read_only, refuse_inside_agent_run, repo_root};
use crate::ctx::Ctx;
use crate::output::{CmdOutput, CmdResult};

const GH_BOARD_TIMEOUT: Duration = Duration::from_secs(30);

/// Resolves `"gh"` through [`aaf_fp::program`] (startup `PATH`, outside the
/// repository/worktree) before delegating execution to [`SystemRunner`].
struct ResolvedGhRunner {
    inner: SystemRunner,
}

impl CommandRunner for ResolvedGhRunner {
    fn run(&self, program: &str, args: &[&str], cwd: &Path) -> io::Result<CommandOutput> {
        if program == "gh" {
            let resolved = aaf_fp::program("gh")?;
            let path = resolved.to_str().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "resolved gh path is not valid UTF-8",
                )
            })?;
            return self.inner.run(path, args, cwd);
        }
        self.inner.run(program, args, cwd)
    }
}

#[derive(Debug, clap::Args)]
pub struct ImportIssuesArgs {
    /// Override `[github].label_filter` for this import.
    #[arg(long)]
    pub label: Option<String>,
    /// Fetch and deduplicate matching issues without writing to the store.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, clap::Args)]
pub struct SyncGithubArgs {
    /// Print the Super-Board column mapping plan without invoking `gh`.
    #[arg(long)]
    pub dry_run: bool,
}

pub fn run_import_issues(args: &ImportIssuesArgs, ctx: &Ctx) -> CmdResult {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf board import-issues") {
        return Ok(refused);
    }
    let repo = repo_root(ctx)?;
    let cfg = ctx.load_config(&repo)?;
    let allowlist = &cfg.global.safety.owner_github_allowlist;
    if !has_trusted_login(allowlist) {
        return Ok(CmdOutput::policy(
            "refused",
            "safety.owner_github_allowlist has no trusted login; add trusted GitHub logins in global config before importing issues",
        ));
    }
    if !args.dry_run && !cfg.repo.github.enabled {
        return Ok(CmdOutput::policy(
            "refused",
            "[github].enabled is false; enable [github] in .autofactory/config.toml before importing issues (or use --dry-run)",
        ));
    }

    let mut github_cfg = cfg.repo.github.clone();
    if let Some(label) = &args.label {
        github_cfg.label_filter = label.trim().to_string();
    }

    let runner = ResolvedGhRunner {
        inner: SystemRunner {
            timeout: GH_BOARD_TIMEOUT,
        },
    };
    let now = SystemClock.now();
    let outcome = if args.dry_run {
        let db = open_db_read_only(ctx)?;
        let store_ref = db.as_ref().map(|d| d as &dyn Store);
        import_issues_dry_run(&runner, store_ref, &repo, allowlist, &github_cfg, now)
    } else {
        let db = open_db(ctx)?;
        import_issues_with_options(&runner, &db, &repo, allowlist, &github_cfg, false, now)
    }
    .map_err(port)?;

    let summary = format!(
        "dry_run={} fetched={} created={} skipped_open={} suppressed_recent={}",
        outcome.dry_run,
        outcome.fetched,
        outcome.created.len(),
        outcome.skipped_open.len(),
        outcome.suppressed_recent.len()
    );
    Ok(CmdOutput::ok("ok", summary)
        .field("dry_run", json!(outcome.dry_run))
        .field("fetched", json!(outcome.fetched))
        .field("created", json!(outcome.created))
        .field("skipped_open", json!(outcome.skipped_open))
        .field("suppressed_recent", json!(outcome.suppressed_recent)))
}

pub fn run_sync_github(args: &SyncGithubArgs, ctx: &Ctx) -> CmdResult {
    if let Some(refused) = refuse_inside_agent_run(ctx, "aaf board sync-github") {
        return Ok(refused);
    }
    let repo = repo_root(ctx)?;
    let cfg = ctx.load_config(&repo)?;
    if !args.dry_run && !cfg.repo.github.enabled {
        return Ok(CmdOutput::policy(
            "refused",
            "[github].enabled is false; enable [github] in .autofactory/config.toml before syncing (or use --dry-run)",
        ));
    }
    let runner = ResolvedGhRunner {
        inner: SystemRunner {
            timeout: GH_BOARD_TIMEOUT,
        },
    };
    let outcome = if args.dry_run {
        match open_db_read_only(ctx)? {
            Some(db) => sync_board(&runner, &db, &repo, &cfg.repo.github, true).map_err(port)?,
            None => SyncOutcome {
                dry_run: true,
                mode: if cfg.repo.github.project_number.is_some() {
                    "project_v2"
                } else {
                    "issue_label"
                }
                .to_string(),
                items: Vec::new(),
                skipped_no_issue: Vec::new(),
                truncated_project_items: false,
            },
        }
    } else {
        let db = open_db(ctx)?;
        sync_board(&runner, &db, &repo, &cfg.repo.github, false).map_err(port)?
    };

    let unchanged_count = outcome.items.iter().filter(|item| item.unchanged).count();
    let error_count = outcome
        .items
        .iter()
        .filter(|item| item.error.is_some())
        .count();

    let mut lines = Vec::with_capacity(outcome.items.len() + 1);
    lines.push(format!(
        "mode={} dry_run={} items={} unchanged={} errors={} skipped_no_issue={} truncated_project_items={}",
        outcome.mode,
        outcome.dry_run,
        outcome.items.len(),
        unchanged_count,
        error_count,
        outcome.skipped_no_issue.len(),
        outcome.truncated_project_items
    ));
    for item in &outcome.items {
        let tag = item
            .issue_number
            .map_or_else(|| "-".to_string(), |n| format!("#{n}"));
        let mut line = format!(
            "  {} {} -> {} (synced={} unchanged={})",
            item.task_id, tag, item.column, item.synced, item.unchanged
        );
        if let Some(item_err) = &item.error {
            line.push_str(&format!(" error={item_err}"));
        }
        lines.push(line);
    }

    let output = if error_count > 0 {
        CmdOutput::policy("partial_error", lines.join("\n"))
    } else {
        CmdOutput::ok("ok", lines.join("\n"))
    };
    Ok(output
        .field("mode", json!(outcome.mode))
        .field("dry_run", json!(outcome.dry_run))
        .field("unchanged", json!(unchanged_count))
        .field("errors", json!(error_count))
        .field(
            "truncated_project_items",
            json!(outcome.truncated_project_items),
        )
        .field(
            "items",
            serde_json::to_value(&outcome.items).map_err(|e| err("items", e))?,
        )
        .field("skipped_no_issue", json!(outcome.skipped_no_issue)))
}
