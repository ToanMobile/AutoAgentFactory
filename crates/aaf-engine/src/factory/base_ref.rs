//! The base branch tasks start from and land on (`gate.base_ref`).

use std::path::Path;

use aaf_domain::ports::{PortError, PortResult};

use super::git_real::err;
use crate::gate::base::{AUTO_FALLBACKS, origin_head_branch, parse_remote_tracking_ref};
use crate::git::cmd::{git, git_raw};
use crate::git::has_remote;

pub(super) const DEFAULT_REMOTE: &str = "origin";

/// The branch tasks start from and land on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseRef {
    /// `Some` when the base is a remote-tracking branch.
    pub remote: Option<String>,
    /// Short branch name, e.g. `main`.
    pub branch: String,
}

impl BaseRef {
    /// What a new task branch is cut from: `origin/main` or `main`.
    pub fn start_point(&self) -> String {
        match &self.remote {
            Some(remote) => format!("{remote}/{}", self.branch),
            None => self.branch.clone(),
        }
    }
}

fn is_commit(repo: &Path, name: &str) -> PortResult<bool> {
    let spec = format!("{name}^{{commit}}");
    Ok(git_raw(repo, &["rev-parse", "--verify", "--quiet", &spec])
        .map_err(err)?
        .success())
}

fn split(repo: &Path, name: &str) -> PortResult<BaseRef> {
    if let Some((remote, branch)) = name.split_once('/')
        && !remote.starts_with('-')
        && has_remote(repo, remote).map_err(err)?
    {
        return Ok(BaseRef {
            remote: Some(remote.to_string()),
            branch: branch.to_string(),
        });
    }
    Ok(BaseRef {
        remote: None,
        branch: name.to_string(),
    })
}

/// Resolves `gate.base_ref` to the base branch. `auto` follows the same
/// order as the gate: `origin/HEAD`, `origin/main`, `origin/master`, `main`,
/// `master`; a repository with none of them uses its current branch.
pub fn resolve_base(repo: &Path, base_ref: &str) -> PortResult<BaseRef> {
    let wanted = base_ref.trim();
    if !wanted.is_empty() && wanted != "auto" {
        if wanted.starts_with('-') || !is_commit(repo, wanted)? {
            return Err(PortError::Invalid(format!(
                "gate.base_ref {wanted:?} does not resolve to a commit"
            )));
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
