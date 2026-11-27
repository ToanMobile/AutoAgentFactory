//! Opens a pull request for a task branch through the `gh` CLI.

use std::io;
use std::path::{Path, PathBuf};

use aaf_domain::gate::{Receipt, Severity};
use aaf_domain::proof::ProofRecord;
use serde::{Deserialize, Serialize};

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

impl PrOutcome {
    /// Parses the positive pull request number from `Opened { url }`, if any.
    pub fn pr_number(&self) -> Option<u64> {
        match self {
            PrOutcome::Opened { url } => parse_pr_number_from_url(url),
            PrOutcome::Skipped { .. } => None,
        }
    }
}

/// Extracts the positive pull request number from a GitHub PR URL
/// (`.../pull/<N>` or `.../pulls/<N>`).
pub fn parse_pr_number_from_url(url: &str) -> Option<u64> {
    let trimmed = url.trim().split(['?', '#']).next()?.trim_end_matches('/');
    let mut parts = trimmed.rsplit('/');
    let num = parts.next()?.parse::<u64>().ok()?;
    let seg = parts.next()?;
    (num > 0 && matches!(seg, "pull" | "pulls")).then_some(num)
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

fn sanitize_md_code(text: &str) -> String {
    text.replace('`', "'")
        .replace(['\r', '\n'], " ")
        .trim()
        .to_string()
}

fn is_non_fast_forward(err: &GitError) -> bool {
    let GitError::Command { stderr, .. } = err else {
        return false;
    };
    let lower = stderr.to_ascii_lowercase();
    lower.contains("non-fast-forward")
        || lower.contains("fetch first")
        || lower.contains("[rejected]")
        || lower.contains("updates were rejected")
}

#[derive(Deserialize)]
struct ExistingPr {
    url: String,
}

fn update_existing_pr(
    runner: &dyn CommandRunner,
    cwd: &Path,
    base: &str,
    branch: &str,
    body: &str,
) -> Result<Option<String>, GitError> {
    let list = runner.run(
        "gh",
        &[
            "pr", "list", "--base", base, "--head", branch, "--state", "open", "--json", "url",
        ],
        cwd,
    )?;
    if !list.success() {
        return Ok(None);
    }
    let prs: Vec<ExistingPr> = serde_json::from_str(&list.stdout).unwrap_or_default();
    let Some(url) = prs
        .into_iter()
        .map(|p| p.url.trim().to_string())
        .find(|u| u.starts_with("http"))
    else {
        return Ok(None);
    };
    let edit = runner.run("gh", &["pr", "edit", &url, "--body", body], cwd)?;
    if !edit.success() {
        return Err(GitError::Command {
            command: "gh pr edit".to_string(),
            code: edit.exit_code,
            stderr: edit.stderr.trim().to_string(),
        });
    }
    Ok(Some(url))
}

/// Pushes the task branch and opens (or updates) a PR against the base.
/// Without the remote or without `gh` nothing is pushed and the outcome is
/// `Skipped`.
pub fn open_pr(req: &PrRequest, runner: &dyn CommandRunner) -> Result<PrOutcome, GitError> {
    open_pr_with_proof(req, None, runner)
}

/// Pushes the task branch and opens (or updates) a PR against the base,
/// optionally embedding a signed device-verification proof summary.
pub fn open_pr_with_proof(
    req: &PrRequest,
    proof: Option<&ProofRecord>,
    runner: &dyn CommandRunner,
) -> Result<PrOutcome, GitError> {
    // Validates the branch is `aaf/<valid_task_id>` before any push.
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
    super::ensure_plain_reflogs_all(&[&req.repo_root, &req.worktree.path])?;
    if let Err(push_err) = git(&req.repo_root, &["push", &req.remote, &refspec]) {
        if is_non_fast_forward(&push_err) {
            git(
                &req.repo_root,
                &["push", "--force-with-lease", &req.remote, &refspec],
            )?;
        } else {
            return Err(push_err);
        }
    }

    // Title and body are untrusted text: each travels as one argv entry.
    let body = pr_body_with_proof(req, proof);
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
        if out.stderr.to_ascii_lowercase().contains("already exists")
            && let Some(url) = update_existing_pr(runner, cwd, &req.base, branch, &body)?
        {
            return Ok(PrOutcome::Opened { url });
        }
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

/// Pull request body wrapping [`pr_body`] plus an optional signed mobile/device proof summary.
pub fn pr_body_with_proof(req: &PrRequest, proof: Option<&ProofRecord>) -> String {
    let mut body = pr_body(&req.receipt);
    if let Some(record) = proof {
        body.push_str("\n## Verification Proof\n\n");
        body.push_str(&format!(
            "- Tool: `{}`\n",
            sanitize_md_code(&record.source_tool)
        ));
        body.push_str(&format!(
            "- Device: `{}`\n",
            sanitize_md_code(&record.device_serial)
        ));
        body.push_str(&format!(
            "- Resolution: {}x{} ({} bytes)\n",
            record.width, record.height, record.byte_len
        ));
        body.push_str(&format!(
            "- Image SHA-256: `{}`\n",
            sanitize_md_code(&record.image_sha256)
        ));
        body.push_str(&format!(
            "- Tree: `{}`\n",
            sanitize_md_code(&record.tree_fp)
        ));
    }
    body
}
