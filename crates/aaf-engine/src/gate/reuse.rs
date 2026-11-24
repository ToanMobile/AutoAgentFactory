//! Signed receipt reuse (local and cross-worktree common receipt cache) and
//! receipt/config hashing helpers.

use std::path::Path;

use aaf_domain::config::{AuditScope, RepoConfig};
use aaf_domain::gate::{GateMode, GateStatus, LockStatus, RECEIPT_SCHEMA_VERSION, Receipt};
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
    aaf_fp::hex(&hasher.finalize())
}

/// Alternate config hash with `require_different_provider = true` so callers
/// enforcing independent approval reuse receipts signed under tier-1 hardening.
pub(crate) fn alt_config_hash(cfg: &RepoConfig, require_independent: bool) -> Option<String> {
    if require_independent && !cfg.cross_audit.require_different_provider {
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
    let common = Path::new(&common);
    let main_root = match common.parent() {
        Some(parent) if common.file_name().is_some_and(|name| name == ".git") => parent,
        _ => common,
    };
    aaf_fp::repo_id(main_root)
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
    let approval_ok = !asked.require_independent_approval
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

/// Writes the local signed receipt and updates or evicts the cross-worktree
/// common receipt cache.
pub(crate) fn persist_receipt(
    data_dir: &Path,
    key: &[u8],
    common_repo_id: &str,
    proof_required: bool,
    receipt: &Receipt,
) -> Result<(), GateError> {
    aaf_fp::write_receipt(data_dir, key, receipt)?;
    if receipt.status == GateStatus::Clear && !proof_required && !has_work_tree_warning(receipt) {
        let _ = aaf_fp::write_common_receipt(data_dir, key, common_repo_id, receipt);
    } else {
        aaf_fp::remove_common_receipt(data_dir, common_repo_id, &receipt.tree_fp);
    }
    Ok(())
}
