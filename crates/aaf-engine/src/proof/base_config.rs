//! Repository `[proof]` configuration read strictly from immutable
//! `git show <base_sha>:.autofactory/config.toml`, never from the worktree.

use std::path::Path;

use aaf_domain::config::{ProofConfig, RepoConfig};

use super::ProofError;
use crate::config::REPO_CONFIG_REL;

pub(super) fn validate_base_sha(base_sha: &str) -> Result<&str, ProofError> {
    let sha = base_sha.trim();
    if sha.is_empty()
        || sha.starts_with('-')
        || sha.contains(':')
        || sha.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(ProofError::Invalid(format!(
            "invalid base_sha {base_sha:?}"
        )));
    }
    Ok(sha)
}

pub(super) fn load_base_repo_config(
    repo_root: &Path,
    base_sha: &str,
) -> Result<Option<RepoConfig>, ProofError> {
    let sha = validate_base_sha(base_sha)?;
    let listed = aaf_fp::git(
        repo_root,
        &["ls-tree", sha, "--", REPO_CONFIG_REL],
        &[],
        aaf_fp::GIT_TIMEOUT,
    )?;
    let line = listed.trim();
    if line.is_empty() {
        return Ok(None);
    }
    let mode = line.split_whitespace().next().unwrap_or("");
    if !matches!(mode, "100644" | "100755") {
        return Err(ProofError::Invalid(format!(
            "{REPO_CONFIG_REL} at {sha} must be a regular file blob (got mode {mode:?})"
        )));
    }
    let spec = format!("{sha}:{REPO_CONFIG_REL}");
    let shown = aaf_fp::git(repo_root, &["show", &spec], &[], aaf_fp::GIT_TIMEOUT)?;
    let repo_cfg: RepoConfig = toml::from_str(&shown)
        .map_err(|err| ProofError::Invalid(format!("invalid TOML in {spec}: {}", err.message())))?;
    Ok(Some(repo_cfg))
}

pub(super) fn load_base_proof_config(
    repo_root: &Path,
    base_sha: &str,
) -> Result<Option<ProofConfig>, ProofError> {
    Ok(load_base_repo_config(repo_root, base_sha)?.map(|cfg| cfg.proof))
}

pub(super) fn normalize_capture_lock_globs(globs: Vec<String>) -> Vec<String> {
    globs
        .into_iter()
        .filter_map(|raw| {
            let mut s = raw.trim();
            loop {
                if let Some(rest) = s.strip_prefix("./") {
                    s = rest;
                } else if let Some(rest) = s.strip_prefix('/') {
                    s = rest;
                } else {
                    break;
                }
            }
            (!s.is_empty()).then(|| s.to_string())
        })
        .collect()
}

/// Loads `[proof].capture_cmd` strictly from immutable
/// `git show <base_sha>:.autofactory/config.toml`.
///
/// Returns `Ok(None)` when `.autofactory/config.toml` does not exist at
/// `base_sha` or its `[proof].capture_cmd` is empty. Fails closed on invalid
/// `base_sha`, non-blob entries, git errors, or invalid TOML.
pub fn load_base_proof_capture_cmd(
    repo_root: &Path,
    base_sha: &str,
) -> Result<Option<String>, ProofError> {
    let Some(proof) = load_base_proof_config(repo_root, base_sha)? else {
        return Ok(None);
    };
    let cmd = proof.capture_cmd.trim();
    Ok((!cmd.is_empty()).then(|| cmd.to_string()))
}
