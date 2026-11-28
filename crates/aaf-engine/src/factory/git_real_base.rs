//! Locked PR-merge and low-level git helpers for [`super::git_real::RealGit`].

use std::path::Path;
use std::time::Duration;

use aaf_domain::config::GlobalConfig;
use aaf_domain::gate::GateMode;
use aaf_domain::ports::{PortError, PortResult};

use super::adapters::SystemClock;
use super::git_real::err;
use crate::git::cmd::{Output, git, git_raw};
use crate::git::pr::parse_pr_number_from_url;
use crate::pr_ops::{PrMergeOutcome, PrMergeRequest, merge_pr};

/// How long one unattended merge attempt waits for CI to register and
/// finish. A PR still pending after it stays in Review with `ci_pending`
/// and the review sweep retries it with backoff.
pub(crate) const FACTORY_CI_WAIT_SECS: u64 = 60;

/// `(ci_wait_secs, require_checks)` of an unattended merge: wait a bounded
/// time for CI, and never read "no check registered" as a pass.
pub(crate) fn factory_ci_policy() -> (u64, bool) {
    (FACTORY_CI_WAIT_SECS, true)
}
use crate::git::{GitError, MergeStrategy, SystemGh, TaskWorktree};

pub(super) const NO_HOOKS: &str = "core.hooksPath=/dev/null";

/// Aborts the rebase in progress in `tree` and returns the files it stopped
/// on. Empty when it stopped for another reason.
pub(super) fn abort_rebase(tree: &Path) -> Result<Vec<String>, GitError> {
    let unmerged = git_raw(tree, &["diff", "--name-only", "--diff-filter=U", "-z"])?;
    let files = unmerged
        .stdout
        .split('\0')
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect();
    git(tree, &["-c", NO_HOOKS, "rebase", "--abort"])?;
    Ok(files)
}

/// F4-A: git in `tree` running no hook; a repo-relative hooks path is agent-written.
pub(super) fn git_no_hooks(tree: &Path, args: &[&str]) -> Result<Output, GitError> {
    git_raw(tree, &[&["-c", NO_HOOKS][..], args].concat())
}

/// O12-R10-3 (d): refuse undeclared nested git repositories staged as bare gitlinks.
pub(super) fn refuse_nested_repositories(tree: &Path) -> PortResult<()> {
    const GITLINK_MODE: &str = "160000";
    let raw = git(tree, &["diff", "--cached", "--raw", "-z"]).map_err(err)?;
    let declared: Vec<String> = git_raw(
        tree,
        &[
            "config",
            "--file",
            ".gitmodules",
            "--get-regexp",
            r"^submodule\..*\.path$",
        ],
    )
    .ok()
    .filter(Output::success)
    .map(|out| {
        out.stdout
            .lines()
            .filter_map(|line| line.split_once(' ').map(|(_, path)| path.to_string()))
            .collect()
    })
    .unwrap_or_default();
    let mut nested = Vec::new();
    let mut fields = raw.split('\0').filter(|f| !f.is_empty());
    while let Some(header) = fields.next() {
        let parts: Vec<&str> = header.trim_start_matches(':').split(' ').collect();
        let status = parts.last().copied().unwrap_or_default();
        let n = if status.starts_with('R') || status.starts_with('C') {
            2
        } else {
            1
        };
        let names: Vec<&str> = fields.by_ref().take(n).collect();
        let new_link = parts.get(1) == Some(&GITLINK_MODE) && parts.first() != Some(&GITLINK_MODE);
        if let (true, Some(name)) = (new_link, names.last())
            && !declared.iter().any(|p| p == name)
        {
            nested.push((*name).to_string());
        }
    }
    if nested.is_empty() {
        return Ok(());
    }
    for path in &nested {
        let spec = format!(":(literal){path}");
        git(tree, &["reset", "--quiet", "--", &spec]).map_err(err)?;
    }
    let list = nested
        .iter()
        .map(|p| format!("`{p}`"))
        .collect::<Vec<_>>()
        .join(", ");
    Err(PortError::Conflict(format!(
        "nothing was committed: {list} is a nested git repository of its own inside the \
         worktree and would be committed as a bare submodule link without its files. \
         Remove that nested repository directory, or recreate its files without a git \
         repository of their own"
    )))
}

fn load_global_config() -> Option<GlobalConfig> {
    let path = crate::config::global_config_path(&|k| std::env::var(k).ok())?;
    let text = std::fs::read_to_string(path).ok()?;
    aaf_domain::builtin_defaults::parse_global(&text).ok()
}

/// Runs [`crate::git::pr_merge::merge_pr`] for an opened PR URL.
/// Callers must NOT hold any `MergeLock` when calling this function.
pub(super) fn merge_pr_real(
    repo_root: &Path,
    locks_dir: &Path,
    remote: &str,
    lock_timeout: Duration,
    worktree: &TaskWorktree,
    pr_url: &str,
    head_sha: &str,
) -> PortResult<PrMergeOutcome> {
    let pr_number = parse_pr_number_from_url(pr_url).ok_or_else(|| {
        PortError::Invalid(format!("could not parse PR number from URL {pr_url:?}"))
    })?;
    let expect_head = if head_sha.trim().is_empty() {
        git(&worktree.path, &["rev-parse", "HEAD"]).map_err(err)?
    } else {
        head_sha.trim().to_string()
    };
    let data_dir = locks_dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| locks_dir.to_path_buf());
    let global_config = load_global_config();
    let machine = global_config
        .as_ref()
        .map(|cfg| cfg.machine.clone())
        .unwrap_or_default();
    let req = PrMergeRequest {
        repo_root: repo_root.to_path_buf(),
        data_dir,
        pr_number,
        expect_head,
        remote: remote.to_string(),
        mode: GateMode::Scoped,
        strategy: MergeStrategy::Squash,
        subject: None,
        body_file: None,
        ci_wait_secs: factory_ci_policy().0,
        require_checks: factory_ci_policy().1,
        lock_timeout,
        dry_run: false,
        machine,
        global_config,
        no_receipt_reuse: false,
        run_commands_despite_head_lock: false,
        strict_merged_head: false,
        keep_non_aaf_local: false,
    };
    crate::git::ensure_plain_reflogs_all(&[repo_root, &worktree.path]).map_err(err)?;
    merge_pr(&req, &SystemGh, &SystemClock, &|_| {}).map_err(err)
}

#[cfg(test)]
mod tests {
    #[test]
    fn unattended_merge_waits_for_ci_and_requires_a_check() {
        let (wait, require) = super::factory_ci_policy();
        assert!(wait > 0 && wait <= 300, "{wait}");
        assert!(require);
    }
}
