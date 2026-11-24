//! Signed receipt reuse (local and cross-worktree common receipt cache) and
//! receipt/config hashing helpers.

use std::path::{Path, PathBuf};

use aaf_domain::config::{AuditScope, RepoConfig};
use aaf_domain::gate::{GateMode, GateStatus, LockStatus, RECEIPT_SCHEMA_VERSION, Receipt};
use aaf_domain::pipeline::canonical_provider;
use sha2::{Digest, Sha256};

use super::{GIT_TIMEOUT, GateError};

const WORK_TREE_WARNING_MARKER: &str = "warning: work tree:";

/// sha256 of the configuration that decides the gate's outcome.
pub fn config_hash(cfg: &RepoConfig) -> String {
    // Plain structs of strings and integers always serialise; the fallback
    // only keeps this function infallible.
    let json = serde_json::to_string(&(&cfg.gate, &cfg.commands, &cfg.proof)).unwrap_or_default();
    if cfg.cross_audit.audit_scope == AuditScope::HeadLock
        && !cfg.cross_audit.score_gate
        && !cfg.cross_audit.require_different_provider
        && !cfg.cross_audit.dual_review
    {
        return aaf_fp::sha256_hex(json.as_bytes());
    }
    let mut hasher = Sha256::new();
    hasher.update(json.as_bytes());
    hasher.update(b"\0cross_audit\0");
    hasher.update(cfg.cross_audit.audit_scope.as_str().as_bytes());
    hasher.update([
        u8::from(cfg.cross_audit.score_gate),
        cfg.cross_audit.min_average_score,
        cfg.cross_audit.min_criterion_score,
        u8::from(cfg.cross_audit.require_different_provider),
    ]);
    if cfg.cross_audit.dual_review {
        hasher.update(b"\0dual_review\0");
        hasher.update([
            u8::from(cfg.cross_audit.dual_review),
            u8::from(cfg.cross_audit.cross_adjudicate),
        ]);
        let mut canonical: Vec<(&str, &str, &str, String, &str, &str)> = cfg
            .cross_audit
            .reviewers
            .iter()
            .map(|r| {
                (
                    r.id.as_str(),
                    r.executor.as_str(),
                    r.executor_cmd.as_str(),
                    canonical_provider(&r.executor),
                    r.model.as_str(),
                    r.effort.as_str(),
                )
            })
            .collect();
        canonical.sort();
        hasher.update((canonical.len() as u64).to_le_bytes());
        for (id, executor, executor_cmd, provider, model, effort) in canonical {
            for field in [id, executor, executor_cmd, provider.as_str(), model, effort] {
                hasher.update((field.len() as u64).to_le_bytes());
                hasher.update(field.as_bytes());
                hasher.update([0u8]);
            }
        }
    }
    aaf_fp::hex(&hasher.finalize())
}

/// Alternate config hash with `require_different_provider = true` so callers
/// with `require_different_provider = false` still reuse strictly stronger
/// receipts signed under tier-1 hardening.
pub(crate) fn alt_config_hash(cfg: &RepoConfig, _require_independent: bool) -> Option<String> {
    if !cfg.cross_audit.require_different_provider {
        let mut strict_cfg = cfg.clone();
        strict_cfg.cross_audit.require_different_provider = true;
        Some(config_hash(&strict_cfg))
    } else {
        None
    }
}

/// Key of the repository in the learned memory statistics. Every worktree
/// of one repository shares it, so a task's fresh worktree starts with the
/// measurements of its siblings instead of as "never measured".
pub fn stats_repo_id(repo_root: &Path) -> String {
    let args = ["rev-parse", "--path-format=absolute", "--git-common-dir"];
    let Ok(common) = aaf_fp::git(repo_root, &args, &[], GIT_TIMEOUT) else {
        return aaf_fp::repo_id(repo_root);
    };
    stats_id_of_common_dir(Path::new(&common))
}

fn stats_id_of_common_dir(common: &Path) -> String {
    let main_root = match common.parent() {
        Some(parent) if common.file_name().is_some_and(|name| name == ".git") => parent,
        _ => common,
    };
    aaf_fp::repo_id(main_root)
}

/// `(aaf_fp::repo_root(dir), stats_repo_id(root))` from one git call where
/// git answers both at once; the two calls otherwise. `None` when `dir` is
/// not in a work tree git can read.
pub fn repo_root_and_stats_id(dir: &Path) -> Option<(PathBuf, String)> {
    repo_root_and_stats_id_by(
        |args| aaf_fp::git(dir, args, &[], GIT_TIMEOUT).ok(),
        || {
            let root = aaf_fp::repo_root(dir).ok()?;
            let id = stats_repo_id(&root);
            Some((root, id))
        },
    )
}

/// [`repo_root_and_stats_id`] with its combined git call and its two-call
/// fallback passed in, so a test can count which one answered.
pub(crate) fn repo_root_and_stats_id_by(
    git: impl FnOnce(&[&str]) -> Option<String>,
    fallback: impl FnOnce() -> Option<(PathBuf, String)>,
) -> Option<(PathBuf, String)> {
    let args = [
        "rev-parse",
        "--show-toplevel",
        "--path-format=absolute",
        "--git-common-dir",
    ];
    if let Some(out) = git(&args)
        && let Some((root, common)) = out.split_once('\n')
        && !root.is_empty()
        && !common.is_empty()
        && !common.contains('\n')
    {
        return Some((
            PathBuf::from(root),
            stats_id_of_common_dir(Path::new(common)),
        ));
    }
    fallback()
}

fn mode_rank(mode: GateMode) -> u8 {
    match mode {
        GateMode::Scoped => 0,
        GateMode::Full => 1,
        GateMode::Release => 2,
    }
}

pub(crate) fn has_work_tree_warning(receipt: &Receipt) -> bool {
    receipt
        .reasons
        .iter()
        .any(|reason| reason.contains(WORK_TREE_WARNING_MARKER))
}

/// What a stored receipt must have been made for to stand in for a new run.
pub(crate) struct Asked<'a> {
    /// Canonical repo root.
    pub repo_root: &'a Path,
    /// Shared repository id across all worktrees of the same repo.
    pub common_repo_id: &'a str,
    pub allow_common_cache: bool,
    pub strict_hidden: bool,
    /// Files hidden from the fingerprint are still on disk (named in the
    /// receipt under [`super::HiddenPolicy::Warn`]): a release gate here
    /// would have blocked on them.
    pub hidden_on_disk: bool,
    pub require_independent_approval: bool,
    pub tree_fp: &'a str,
    /// The base as it resolves now.
    pub base_sha: Option<&'a str>,
    pub config_hash: &'a str,
    /// Optional alternate config hash (with `require_different_provider = true`)
    /// so cross-worktree callers match receipts produced under tier-1 hardening.
    pub alt_config_hash: Option<&'a str>,
    pub mode: GateMode,
}

pub(crate) fn receipt_matches(stored: &Receipt, asked: &Asked<'_>, check_repo_root: bool) -> bool {
    let config_ok = stored.config_hash == asked.config_hash
        || asked
            .alt_config_hash
            .is_some_and(|alt| stored.config_hash == alt);
    let need_ind = asked.require_independent_approval || stored.head_lock.has_hard_lock();
    let approval_ok = !need_ind
        || stored.head_lock.status == LockStatus::Clear
        || (stored.head_lock.status == LockStatus::Approved
            && stored
                .head_lock
                .approval
                .as_ref()
                .is_some_and(|approval| !approval.same_account));
    stored.schema_version == RECEIPT_SCHEMA_VERSION
        && stored.status == GateStatus::Clear
        && (!check_repo_root || Path::new(&stored.repo_root) == asked.repo_root)
        && stored.tree_fp == asked.tree_fp
        && stored.base_sha.as_deref() == asked.base_sha
        && config_ok
        && approval_ok
        && mode_rank(stored.mode) >= mode_rank(asked.mode)
        && (!asked.strict_hidden || !has_work_tree_warning(stored))
}

/// A sibling's release receipt written here would unlock a store submission
/// of a tree that a release gate in this checkout would block.
fn release_beside_hidden(stored: &Receipt, asked: &Asked<'_>) -> bool {
    asked.hidden_on_disk && stored.mode == GateMode::Release && asked.mode != GateMode::Release
}

/// The stored receipt, when it already proves what is being asked: signed,
/// `Clear`, for exactly this repo root (or sibling worktree of the same repo
/// when proof is not required), tree, base and gate configuration, from a mode
/// at least as strict. Anything else, a tampered file included, is a miss.
pub(crate) fn reusable_receipt(data_dir: &Path, key: &[u8], asked: &Asked<'_>) -> Option<Receipt> {
    let local_path = aaf_fp::receipt_path(data_dir, asked.repo_root);
    if local_path.exists() {
        // Fail closed on a tampered or foreign local receipt: never fall back
        // to the cross-worktree cache when this checkout's own receipt is bad.
        let stored = aaf_fp::read_receipt(data_dir, key, asked.repo_root).ok()?;
        if Path::new(&stored.repo_root) != asked.repo_root {
            return None;
        }
        if receipt_matches(&stored, asked, true) {
            return Some(stored);
        }
    }
    if asked.allow_common_cache
        && let Ok(mut stored) =
            aaf_fp::read_common_receipt(data_dir, key, asked.common_repo_id, asked.tree_fp)
        && receipt_matches(&stored, asked, false)
        && !release_beside_hidden(&stored, asked)
    {
        stored.repo_root = asked.repo_root.to_string_lossy().into_owned();
        // Materialize locally so hooks/handover reading `receipt_path(repo_root)` see it.
        let _ = aaf_fp::write_receipt(data_dir, key, &stored);
        return Some(stored);
    }
    None
}

/// Probes the cross-worktree common receipt cache for an exact tree fingerprint
/// and base commit without checking out a working tree.
pub(crate) fn probe_common_receipt(
    data_dir: &Path,
    repo_root: &Path,
    cfg: &RepoConfig,
    mode: GateMode,
    tree_fp: &str,
    base_sha: &str,
    require_independent_approval: bool,
) -> Option<Receipt> {
    if cfg.proof.required {
        return None;
    }
    let key = aaf_fp::load_key(data_dir).ok()?;
    let common_repo_id = stats_repo_id(repo_root);
    let cfg_hash = config_hash(cfg);
    let alt_hash = alt_config_hash(cfg, require_independent_approval);
    let asked = Asked {
        repo_root,
        common_repo_id: &common_repo_id,
        allow_common_cache: true,
        strict_hidden: true,
        // No work tree is read or written here (the receipt is not
        // materialized beside one), so no hidden file is on disk.
        hidden_on_disk: false,
        require_independent_approval,
        tree_fp,
        base_sha: Some(base_sha),
        config_hash: &cfg_hash,
        alt_config_hash: alt_hash.as_deref(),
        mode,
    };
    let stored = aaf_fp::read_common_receipt(data_dir, &key, &common_repo_id, tree_fp).ok()?;
    receipt_matches(&stored, &asked, false).then_some(stored)
}

/// Returns `true` only when `repo_root` has zero gitignored untracked files
/// (such as build artifacts or agent-planted `.gitignore`d markers like
/// `target/.aaf_pass`). Fails closed (`false`) on any git error or timeout.
pub(crate) fn worktree_has_no_ignored_files(repo_root: &Path) -> bool {
    matches!(
        aaf_fp::git(
            repo_root,
            &[
                "ls-files",
                "--others",
                "--ignored",
                "--exclude-standard",
                "--directory",
                "-z",
            ],
            &[],
            GIT_TIMEOUT,
        ),
        Ok(out) if out.is_empty()
    )
}

/// Writes the local signed receipt and updates or evicts the cross-worktree
/// common receipt cache. A `common_receipt` is only published when
/// `write_common` is `true` (trusted merge verifier or clean 0-ignored-file
/// worktree).
pub(crate) fn persist_receipt(
    data_dir: &Path,
    key: &[u8],
    common_repo_id: &str,
    proof_required: bool,
    write_common: bool,
    receipt: &Receipt,
) -> Result<(), GateError> {
    aaf_fp::write_receipt(data_dir, key, receipt)?;
    if receipt.status == GateStatus::Clear && !proof_required && !has_work_tree_warning(receipt) {
        if write_common {
            let _ = aaf_fp::write_common_receipt(data_dir, key, common_repo_id, receipt);
        }
    } else {
        aaf_fp::remove_common_receipt(data_dir, common_repo_id, &receipt.tree_fp);
    }
    Ok(())
}
