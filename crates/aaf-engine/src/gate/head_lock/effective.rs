//! The head-lock verdict on an explicit change list, and the changes it was
//! judged on (the list plus the ignored files the capture globs add).

use std::path::Path;

use aaf_domain::config::RepoConfig;
use aaf_domain::gate::{HeadLockReport, LockStatus, LockTier};

use super::{content_hash, locked_with, valid_approval};
use crate::gate::changes::{self, AddedLines, Change};
use crate::gate::{GIT_TIMEOUT, GateError};

#[allow(clippy::too_many_arguments)]
pub(crate) fn evaluate_changes_with_common(
    data_dir: &Path,
    repo_root: &Path,
    common_root: Option<&Path>,
    cfg: &RepoConfig,
    extra_hard_globs: &[String],
    (base, changes): (&str, &[Change]),
    added: &[AddedLines],
    require_independent: bool,
) -> Result<HeadLockReport, GateError> {
    evaluate_changes_effective(
        data_dir,
        repo_root,
        common_root,
        cfg,
        extra_hard_globs,
        (base, changes),
        added,
        require_independent,
    )
    .map(|(report, _)| report)
}

/// [`evaluate_changes_with_common`] that also returns the changes the lock was
/// judged on: `changes` plus the ignored files `extra_hard_globs` match
/// (`untracked` is left empty: only the effective `changes` are known here).
#[allow(clippy::too_many_arguments)]
pub(crate) fn evaluate_changes_effective(
    data_dir: &Path,
    repo_root: &Path,
    common_root: Option<&Path>,
    cfg: &RepoConfig,
    extra_hard_globs: &[String],
    (base, changes): (&str, &[Change]),
    added: &[AddedLines],
    require_independent: bool,
) -> Result<(HeadLockReport, changes::ChangeSet), GateError> {
    let effective =
        changes::with_ignored_matching(repo_root, extra_hard_globs, changes, GIT_TIMEOUT)?;
    let mut hash = |path: &str| content_hash(repo_root, path);
    let (locked_changes, lock_fp, _) = locked_with(
        (repo_root, base),
        cfg,
        extra_hard_globs,
        &effective,
        added,
        &mut hash,
    )?;
    let effective = changes::ChangeSet {
        changes: effective.into_owned(),
        untracked: Vec::new(),
    };
    if locked_changes.is_empty() {
        let report = HeadLockReport {
            status: LockStatus::Clear,
            lock_fp,
            locked_changes,
            approval: None,
        };
        return Ok((report, effective));
    }
    let has_hard_lock = locked_changes.iter().any(|c| c.tier == LockTier::Hard);
    let need_independent =
        require_independent || cfg.cross_audit.require_different_provider || has_hard_lock;
    let approval = valid_approval(data_dir, repo_root, common_root, &lock_fp, need_independent);
    let report = HeadLockReport {
        status: if approval.is_some() {
            LockStatus::Approved
        } else {
            LockStatus::Blocked
        },
        lock_fp,
        locked_changes,
        approval,
    };
    Ok((report, effective))
}
