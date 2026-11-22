//! Opens a pull request for a task branch through the `gh` CLI.

use std::io;
use std::path::PathBuf;

use aaf_domain::gate::{Receipt, Severity};
use serde::Serialize;

use super::cmd::git;
use super::worktree::TaskWorktree;
use super::{GitError, check_branch_name, has_remote};
use crate::process::CommandRunner;

#[derive(Debug, Clone)]
pub struct PrRequest {
    /// Main work tree of the repository.
    pub repo_root: PathBuf,
    pub worktree: TaskWorktree,
    /// Short name of the branch the PR targets.
    pub base: String,
    pub remote: String,
    pub title: String,
    /// The gate receipt summarised in the PR body.
    pub receipt: Receipt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrOutcome {
    Opened {
        url: String,
    },
    /// No PR could be opened; the branch and the receipt stay for a human.
    Skipped {
        reason: String,
    },
}

/// The serde name of a status enum (`CLEAR`, `scoped`, ...), so the body
/// uses the same words as the receipt file.
fn label<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

/// Markdown summary of the receipt for the PR body.
pub fn pr_body(receipt: &Receipt) -> String {
    let count = |severity: Severity| {
        receipt
            .static_findings
            .iter()
            .filter(|f| f.severity == severity)
            .count()
    };
    let mut body = String::from("## Gate receipt\n\n");
    body.push_str(&format!(
        "- Status: **{}** (mode: {})\n",
        label(&receipt.status),
        label(&receipt.mode)
    ));
    body.push_str(&format!("- Tree: `{}`\n", receipt.tree_fp));
    if let Some(head) = &receipt.head_sha {
        body.push_str(&format!("- Head: `{head}`\n"));
    }
    body.push_str(&format!(
        "- Head lock: {} ({} locked change(s))\n",
        label(&receipt.head_lock.status),
        receipt.head_lock.locked_changes.len()
    ));
    body.push_str(&format!(
        "- Static findings: {} critical, {} warn, {} info ({} suppressed)\n",
        count(Severity::Critical),
        count(Severity::Warn),
        count(Severity::Info),
        receipt.suppressions
    ));
    body.push_str(&format!(
        "- Store compliance: {} ({} critical, {} warnings)\n",
        label(&receipt.store_compliance.status),
        receipt.store_compliance.critical,
        receipt.store_compliance.warnings
    ));
    body.push_str("\n### Commands\n\n");
    if receipt.commands.is_empty() {
        body.push_str("- none\n");
    }
    for command in &receipt.commands {
        let result = match (command.timed_out, command.exit_code) {
            (true, _) => "timed out".to_string(),
            (false, Some(code)) => format!("exit {code}"),
            (false, None) => "no exit code".to_string(),
        };
        // A backtick inside the command would end the code span early.
        let line = command.command.replace('`', "'");
        body.push_str(&format!("- {}: {result} (`{line}`)\n", command.name));
    }
    if !receipt.reasons.is_empty() {
        body.push_str("\n### Reasons\n\n");
        for reason in &receipt.reasons {
            body.push_str(&format!("- {reason}\n"));
        }
    }
    body
}

fn skipped(reason: &str) -> Result<PrOutcome, GitError> {
    Ok(PrOutcome::Skipped {
        reason: reason.to_string(),
    })
}

/// Pushes the task branch and opens a PR against the base.
/// Without the remote or without `gh` nothing is pushed and the outcome is
/// `Skipped`.
pub fn open_pr(req: &PrRequest, runner: &dyn CommandRunner) -> Result<PrOutcome, GitError> {
    req.worktree.task_id()?;
    check_branch_name(&req.repo_root, &req.base)?;
    if !has_remote(&req.repo_root, &req.remote)? {
        return skipped("the repository has no remote");
    }
    let cwd = req.worktree.path.as_path();
    match runner.run("gh", &["--version"], cwd) {
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return skipped("the gh CLI is not installed");
        }
        Err(err) => return Err(err.into()),
    }

    let branch = req.worktree.branch.as_str();
    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    git(&req.repo_root, &["push", &req.remote, &refspec])?;

    // Title and body are untrusted text: each travels as one argv entry.
    let body = pr_body(&req.receipt);
    let args = [
        "pr", "create", "--title", &req.title, "--body", &body, "--base", &req.base, "--head",
        branch,
    ];
    let out = runner.run("gh", &args, cwd)?;
    let failed = |stderr: &str| GitError::Command {
        command: "gh pr create".to_string(),
        code: out.exit_code,
        stderr: stderr.trim().to_string(),
    };
    if !out.success() {
        return Err(failed(&out.stderr));
    }
    // gh prints the PR URL as its last line.
    match out.stdout.lines().rev().find(|l| l.starts_with("http")) {
        Some(url) => Ok(PrOutcome::Opened {
            url: url.trim().to_string(),
        }),
        None => Err(failed("gh printed no pull request URL")),
    }
}
