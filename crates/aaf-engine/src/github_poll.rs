//! Periodic GitHub issue intake for the unattended run mode: the same
//! fail-closed import as `aaf board import-issues` (owner allowlist only,
//! open/recently-closed dedup), at most once per [`ISSUE_POLL_SECS`].

use std::path::{Path, PathBuf};

use aaf_domain::config::GithubBoardConfig;

use crate::factory::Store;
use crate::github_board::{has_trusted_login, import_issues_with_options};
use crate::process::CommandRunner;

/// Seconds between two issue imports of one run.
pub const ISSUE_POLL_SECS: i64 = 300;

pub struct IssuePoller {
    repo_root: PathBuf,
    allowlist: Vec<String>,
    cfg: GithubBoardConfig,
    last_poll: Option<i64>,
}

impl IssuePoller {
    /// `None` (no polling) unless `[github].enabled` and the owner allowlist
    /// has a trusted login: issues from anyone else never become tasks.
    pub fn new(repo_root: &Path, allowlist: &[String], cfg: &GithubBoardConfig) -> Option<Self> {
        if !cfg.enabled || !has_trusted_login(allowlist) {
            return None;
        }
        Some(IssuePoller {
            repo_root: repo_root.to_path_buf(),
            allowlist: allowlist.to_vec(),
            cfg: cfg.clone(),
            last_poll: None,
        })
    }

    /// Imports new owner issues when the throttle allows. A failure is
    /// returned for the caller to report and is tried again after the
    /// throttle: it never stops the run.
    pub fn tick(&mut self, gh: &dyn CommandRunner, store: &dyn Store, now: i64) -> Option<String> {
        if self
            .last_poll
            .is_some_and(|last| now.saturating_sub(last) < ISSUE_POLL_SECS)
        {
            return None;
        }
        self.last_poll = Some(now);
        import_issues_with_options(
            gh,
            store,
            &self.repo_root,
            &self.allowlist,
            &self.cfg,
            false,
            now,
        )
        .err()
        .map(|e| e.to_string())
    }
}
