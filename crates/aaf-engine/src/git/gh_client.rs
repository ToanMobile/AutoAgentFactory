//! Typed GitHub CLI (`gh`) client for `aaf merge`, abstracted behind
//! [`GhClient`] so integration tests can run real git merges without network.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{GitError, path_str};
use crate::process::run_with_timeout;

const GH_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MergeStrategy {
    #[default]
    Squash,
    Merge,
    Rebase,
}

impl MergeStrategy {
    pub fn gh_flag(self) -> &'static str {
        match self {
            MergeStrategy::Squash => "--squash",
            MergeStrategy::Merge => "--merge",
            MergeStrategy::Rebase => "--rebase",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct MergeCommitRef {
    #[serde(default)]
    pub oid: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrInfo {
    pub number: u64,
    pub state: String,
    pub head_ref_oid: String,
    pub head_ref_name: String,
    pub base_ref_name: String,
    #[serde(default)]
    pub mergeable: String,
    #[serde(default)]
    pub is_draft: bool,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub is_cross_repository: bool,
    #[serde(default)]
    pub merge_commit: Option<MergeCommitRef>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PrCheck {
    pub name: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub bucket: String,
}

/// External GitHub CLI operations; faked in integration tests.
pub trait GhClient: Sync {
    fn view_pr(&self, repo_root: &Path, pr_number: u64) -> Result<PrInfo, GitError>;
    fn pr_checks(&self, repo_root: &Path, pr_number: u64) -> Result<Vec<PrCheck>, GitError>;
    fn merge_pr(
        &self,
        repo_root: &Path,
        pr_number: u64,
        expect_head: &str,
        strategy: MergeStrategy,
        subject: Option<&str>,
        body_file: Option<&Path>,
    ) -> Result<(), GitError>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemGh;

fn run_gh(repo_root: &Path, args: &[&str]) -> Result<String, GitError> {
    let gh_bin = aaf_fp::program("gh")?;
    let mut cmd = Command::new(gh_bin);
    cmd.args(args).current_dir(repo_root);
    let label = format!("gh {}", args.join(" "));
    let finished = run_with_timeout(cmd, GH_TIMEOUT, true)?;
    let Some(status) = finished.status else {
        return Err(GitError::Timeout { command: label });
    };
    let code = status.code();
    if code == Some(0) {
        Ok(String::from_utf8_lossy(&finished.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&finished.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&finished.stdout).trim().to_string();
        let detail = if !stderr.is_empty() { stderr } else { stdout };
        Err(GitError::Command {
            command: label,
            code,
            stderr: detail,
        })
    }
}

impl GhClient for SystemGh {
    fn view_pr(&self, repo_root: &Path, pr_number: u64) -> Result<PrInfo, GitError> {
        let pr = pr_number.to_string();
        let out = run_gh(
            repo_root,
            &[
                "pr",
                "view",
                &pr,
                "--json",
                "number,state,headRefOid,headRefName,baseRefName,mergeable,isDraft,title,url,isCrossRepository,mergeCommit",
            ],
        )?;
        serde_json::from_str(&out)
            .map_err(|e| GitError::Invalid(format!("unparsable gh pr view JSON: {e}")))
    }

    fn pr_checks(&self, repo_root: &Path, pr_number: u64) -> Result<Vec<PrCheck>, GitError> {
        let pr = pr_number.to_string();
        let gh_bin = aaf_fp::program("gh")?;
        let mut cmd = Command::new(gh_bin);
        cmd.args(["pr", "checks", &pr, "--json", "name,state,bucket"])
            .current_dir(repo_root);
        let finished = run_with_timeout(cmd, GH_TIMEOUT, true)?;
        let Some(status) = finished.status else {
            return Err(GitError::Timeout {
                command: format!("gh pr checks {pr}"),
            });
        };
        let stdout = String::from_utf8_lossy(&finished.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&finished.stderr).trim().to_string();
        // `gh pr checks` exits 1 when no checks exist on the PR; treat that as empty list.
        if stdout.is_empty() && stderr.to_ascii_lowercase().contains("no checks reported") {
            return Ok(Vec::new());
        }
        if stdout.is_empty() {
            return Err(GitError::Command {
                command: format!("gh pr checks {pr}"),
                code: status.code(),
                stderr,
            });
        }
        serde_json::from_str(&stdout)
            .map_err(|e| GitError::Invalid(format!("unparsable gh pr checks JSON: {e}")))
    }

    fn merge_pr(
        &self,
        repo_root: &Path,
        pr_number: u64,
        expect_head: &str,
        strategy: MergeStrategy,
        subject: Option<&str>,
        body_file: Option<&Path>,
    ) -> Result<(), GitError> {
        let pr = pr_number.to_string();
        let mut args: Vec<String> = vec![
            "pr".into(),
            "merge".into(),
            pr,
            strategy.gh_flag().into(),
            "--match-head-commit".into(),
            expect_head.into(),
        ];
        if let Some(subj) = subject {
            args.push("--subject".into());
            args.push(subj.into());
        }
        if let Some(body) = body_file {
            args.push("--body-file".into());
            args.push(path_str(body)?.into());
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        run_gh(repo_root, &refs)?;
        Ok(())
    }
}
