//! Whether a worktree's diff needs a verified device proof.
//!
//! This lives in `gate`, not `proof`: it reads the diff through the gate's
//! base resolution and change collection, and `proof` must not depend on
//! `gate` (the gate already depends on `proof`).

use std::path::Path;

use aaf_domain::config::ProofConfig;

use super::{GIT_TIMEOUT, base, changes, proc::git_bytes};
use crate::proof::proof_triggered;

/// Returns `true` when `cfg` requires a verified proof for the diff in `worktree`.
///
/// Mirrors [`super::run_gate`]: returns `false` when `!cfg.required`, `true` when
/// `cfg.trigger_globs` is empty, and otherwise inspects the worktree diff
/// against `base_sha` (or `base_ref`) with [`proof_triggered`], failing closed
/// to `true` if git inspection fails.
pub fn proof_needed_for_worktree(
    worktree: &Path,
    cfg: &ProofConfig,
    base_ref: &str,
    base_sha: Option<&str>,
) -> bool {
    if !cfg.required {
        return false;
    }
    if cfg.trigger_globs.is_empty() {
        return true;
    }
    let target = match base_sha.map(str::trim).filter(|s| !s.is_empty()) {
        Some(sha) => sha.to_string(),
        None => match base::resolve(worktree, base_ref, GIT_TIMEOUT) {
            Ok(base) => base.diff_target,
            Err(_) => return true,
        },
    };
    let Ok(set) = changes::collect(worktree, &target, GIT_TIMEOUT) else {
        return true;
    };
    let changed_paths: Vec<String> = set
        .changes
        .iter()
        .flat_map(|ch| std::iter::once(ch.path.clone()).chain(ch.old_path.iter().cloned()))
        .collect();
    proof_triggered(cfg, &changed_paths)
}

fn is_hex_sha(s: &str) -> bool {
    (7..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Returns `true` when `cfg` requires a verified proof for the committed diff
/// between `base_sha` and `head_sha` in `repo` (e.g. after a merged worktree
/// has been cleaned up). Fails closed to `true` if git inspection fails.
pub fn proof_needed_for_commit_range(
    repo: &Path,
    cfg: &ProofConfig,
    base_sha: &str,
    head_sha: &str,
) -> bool {
    if !cfg.required {
        return false;
    }
    if cfg.trigger_globs.is_empty() {
        return true;
    }
    let (base, head) = (base_sha.trim(), head_sha.trim());
    if !is_hex_sha(base) || !is_hex_sha(head) {
        return true;
    }
    let Ok(raw) = git_bytes(
        repo,
        &["diff", "--name-status", "-M", "-z", base, head, "--"],
        GIT_TIMEOUT,
    ) else {
        return true;
    };
    let mut fields = raw
        .split(|b| *b == 0)
        .filter(|field| !field.is_empty())
        .map(|field| String::from_utf8_lossy(field).into_owned());
    let mut changed_paths = Vec::new();
    while let Some(status) = fields.next() {
        let Some(first) = fields.next() else {
            return true;
        };
        match status.chars().next() {
            Some('A' | 'M' | 'T' | 'U' | 'D') => changed_paths.push(first),
            Some('R' | 'C') => {
                let Some(second) = fields.next() else {
                    return true;
                };
                changed_paths.push(first);
                changed_paths.push(second);
            }
            _ => return true,
        }
    }
    proof_triggered(cfg, &changed_paths)
}
