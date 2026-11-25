//! The base branch tasks start from and land on (`gate.base_ref`).

use std::path::Path;

use aaf_domain::ports::{PortError, PortResult};

use super::cmd::{git, git_raw};
use super::{GitError, has_remote};
use crate::gate::base::{AUTO_FALLBACKS, origin_head_branch, parse_remote_tracking_ref};

pub(crate) const DEFAULT_REMOTE: &str = "origin";

fn err(error: GitError) -> PortError {
    PortError::Io(error.to_string())
}

/// The branch tasks start from and land on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseRef {
    /// `Some` when the base is a remote-tracking branch.
    pub remote: Option<String>,
    /// Short branch name, e.g. `main`; for an explicit base that is no
    /// branch, the full commit SHA it was pinned to.
    pub branch: String,
}

fn is_full_oid(name: &str) -> bool {
    matches!(name.len(), 40 | 64) && name.bytes().all(|b| b.is_ascii_hexdigit())
}

impl BaseRef {
    /// What a new task branch is cut from, as a full ref name
    /// (`refs/remotes/origin/main`, `refs/heads/main`) so a tag called
    /// `main` or `origin/main` cannot shadow it (`gitrevisions(7)`). A
    /// pinned SHA, `HEAD` or a full ref is kept as is.
    pub fn start_point(&self) -> String {
        match &self.remote {
            Some(remote) => format!("refs/remotes/{remote}/{}", self.branch),
            None if self.branch.starts_with("refs/")
                || self.branch == "HEAD"
                || is_full_oid(&self.branch) =>
            {
                self.branch.clone()
            }
            None => format!("refs/heads/{}", self.branch),
        }
    }
}

fn is_commit(repo: &Path, name: &str) -> PortResult<bool> {
    let spec = format!("{name}^{{commit}}");
    Ok(git_raw(repo, &["rev-parse", "--verify", "--quiet", &spec])
        .map_err(err)?
        .success())
}

/// `full` is an existing ref, not a rev expression.
fn ref_exists(repo: &Path, full: &str) -> PortResult<bool> {
    Ok(git_raw(repo, &["show-ref", "--verify", "--quiet", full])
        .map_err(err)?
        .success())
}

/// `full` is an existing ref naming a commit.
fn is_branch_ref(repo: &Path, full: &str) -> PortResult<bool> {
    Ok(ref_exists(repo, full)? && is_commit(repo, full)?)
}

fn invalid(name: &str, why: &str) -> PortError {
    PortError::Invalid(format!("gate.base_ref {name:?} {why}"))
}

fn branch_of(remote: Option<&str>, branch: &str) -> BaseRef {
    BaseRef {
        remote: remote.map(str::to_string),
        branch: branch.to_string(),
    }
}

/// Classifies an explicit `gate.base_ref` against the repository: a
/// remote-tracking branch, a local branch, or else any other commit
/// (`refs/tags/v1`, short SHA, `HEAD~1`) pinned to its full SHA. A short
/// name never falls through to a tag: `<remote>/<b>` without its tracking
/// ref, or a bare tag name, is refused rather than resolved via
/// `refs/tags/` (`gitrevisions(7)` tries tags first).
fn split(repo: &Path, name: &str) -> PortResult<BaseRef> {
    if let Some((remote, branch)) = parse_remote_tracking_ref(name)
        && has_remote(repo, remote).map_err(err)?
        && is_branch_ref(repo, name)?
    {
        return Ok(branch_of(Some(remote), branch));
    }
    if let Some(branch) = name.strip_prefix("refs/heads/")
        && is_branch_ref(repo, name)?
    {
        return Ok(branch_of(None, branch));
    }
    if let Some((remote, branch)) = name.split_once('/')
        && !remote.is_empty()
        && !remote.starts_with('-')
        && has_remote(repo, remote).map_err(err)?
    {
        if is_branch_ref(repo, &format!("refs/remotes/{remote}/{branch}"))? {
            return Ok(branch_of(Some(remote), branch));
        }
        return Err(invalid(
            name,
            &format!("has no remote-tracking ref refs/remotes/{remote}/{branch}"),
        ));
    }
    if is_branch_ref(repo, &format!("refs/heads/{name}"))? {
        return Ok(branch_of(None, name));
    }
    let explicit = name.starts_with("refs/") || name == "HEAD" || is_full_oid(name);
    if !explicit && ref_exists(repo, &format!("refs/tags/{name}"))? {
        return Err(invalid(
            name,
            &format!("names a tag; write refs/tags/{name} to pin it"),
        ));
    }
    let spec = format!("{name}^{{commit}}");
    let out = git_raw(repo, &["rev-parse", "--verify", "--quiet", &spec]).map_err(err)?;
    let sha = out.stdout.trim();
    if !out.success() || sha.is_empty() {
        return Err(invalid(name, "does not resolve to a commit"));
    }
    Ok(branch_of(None, sha))
}

/// Resolves `gate.base_ref` to the base branch. `auto` follows the same
/// order as the gate: `origin/HEAD`, `origin/main`, `origin/master`, `main`,
/// `master`; a repository with none of them uses its current branch.
pub fn resolve_base(repo: &Path, base_ref: &str) -> PortResult<BaseRef> {
    let wanted = base_ref.trim();
    if !wanted.is_empty() && wanted != "auto" {
        // No short-name `is_commit` pre-check: a tag `main` naming a tree
        // would refuse a valid `refs/heads/main`. `split` checks full refs.
        if wanted.starts_with('-') {
            return Err(invalid(wanted, "does not resolve to a commit"));
        }
        return split(repo, wanted);
    }
    let head = git_raw(
        repo,
        &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
    )
    .map_err(err)?;
    let origin_head = head.stdout.trim().to_string();
    let mut candidates = Vec::new();
    // As in the gate: `origin/HEAD` only as a symref to a non-task branch
    // of `origin`, and full ref names a tag or branch cannot shadow.
    if head.success() && origin_head_branch(&origin_head).is_some() {
        candidates.push(origin_head);
    }
    candidates.extend(AUTO_FALLBACKS.map(String::from));
    for name in candidates {
        if is_commit(repo, &name)? {
            let remote = name.strip_prefix("refs/remotes/origin/");
            let branch = remote.or(name.strip_prefix("refs/heads/"));
            return Ok(BaseRef {
                remote: remote.map(|_| DEFAULT_REMOTE.to_string()),
                branch: branch.unwrap_or(&name).to_string(),
            });
        }
    }
    // Probe `@{upstream}` (e.g. `refs/remotes/origin/develop`), rejecting
    // `aaf/*` task branches, before falling back to the current local branch.
    let upstream =
        git_raw(repo, &["rev-parse", "--symbolic-full-name", "@{upstream}"]).map_err(err)?;
    if upstream.success() {
        let full = upstream.stdout.trim();
        if let Some((remote, branch)) = parse_remote_tracking_ref(full)
            && is_commit(repo, full)?
        {
            return Ok(BaseRef {
                remote: Some(remote.to_string()),
                branch: branch.to_string(),
            });
        }
    }
    let current = git(repo, &["symbolic-ref", "--short", "HEAD"]).map_err(err)?;
    Ok(BaseRef {
        remote: None,
        branch: current,
    })
}
