//! Head lock: existing tests, CI and build files may not change without a
//! signed approval of that exact set of changes.

use std::fs;
use std::path::Path;

use aaf_domain::config::{AGENT_CONFIG_GLOBS, RepoConfig};
use aaf_domain::gate::{Approval, ChangeKind, HeadLockReport, LockStatus, LockedChange};
use aaf_domain::ports::Clock;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use super::changes::{self, Change};
use super::{GIT_TIMEOUT, GateError, base};

fn glob_set(globs: &[String]) -> Result<GlobSet, GateError> {
    let mut builder = GlobSetBuilder::new();
    for glob in globs {
        let glob = GlobBuilder::new(glob)
            .literal_separator(true)
            .build()
            // A glob that does not parse would silently unlock files.
            .map_err(|err| GateError::Config(format!("gate.head_lock_globs: {err}")))?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|err| GateError::Config(format!("gate.head_lock_globs: {err}")))
}

/// Hash of what is on disk at `path` now, or `deleted`.
fn content_hash(repo_root: &Path, path: &str) -> Result<String, GateError> {
    let full = repo_root.join(path);
    let Ok(meta) = fs::symlink_metadata(&full) else {
        return Ok("deleted".to_string());
    };
    if meta.is_symlink() {
        let target = fs::read_link(&full)?;
        let target = target.as_os_str().as_encoded_bytes();
        return Ok(format!("link:{}", aaf_fp::sha256_hex(target)));
    }
    if meta.is_file() {
        return Ok(aaf_fp::sha256_hex(&fs::read(&full)?));
    }
    Err(GateError::Config(format!(
        "locked path {path} is neither a file nor a symlink"
    )))
}

fn status_letter(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "A",
        ChangeKind::Modified => "M",
        ChangeKind::Deleted => "D",
        ChangeKind::Renamed => "R",
    }
}

/// The path `change` is locked under, if it is locked. A rename is judged by
/// where the file used to live. New files are exempt (adding a test is
/// always allowed), except what Claude Code loads as instructions, hooks and
/// servers: in a repository that had none, adding them is the change.
fn locked_path<'a>(change: &'a Change, globs: &GlobSet, agent_config: &GlobSet) -> Option<&'a str> {
    if change.kind == ChangeKind::Added && !agent_config.is_match(&change.path) {
        return None;
    }
    let path = change.old_path.as_deref().unwrap_or(&change.path);
    globs.is_match(path).then_some(path)
}

/// Locked changes and their fingerprint (empty when nothing is locked).
fn locked(
    repo_root: &Path,
    cfg: &RepoConfig,
    changes: &[Change],
) -> Result<(Vec<LockedChange>, String), GateError> {
    let globs = glob_set(&cfg.gate.head_lock_globs)?;
    let agent_config: Vec<String> = AGENT_CONFIG_GLOBS
        .iter()
        .map(|g| (*g).to_string())
        .collect();
    let agent_config = glob_set(&agent_config)?;
    let mut entries: Vec<(String, &'static str, String, String)> = Vec::new();
    for change in changes {
        let Some(locked_path) = locked_path(change, &globs, &agent_config) else {
            continue;
        };
        // The hash follows the content on disk, so an approval stops
        // covering the change the moment the file is edited again.
        let hash = content_hash(repo_root, &change.path)?;
        entries.push((
            locked_path.to_string(),
            status_letter(change.kind),
            change.path.clone(),
            hash,
        ));
    }
    if entries.is_empty() {
        return Ok((Vec::new(), String::new()));
    }
    entries.sort();
    let mut material = Vec::new();
    for (path, status, current, hash) in &entries {
        for field in [path.as_str(), status, current.as_str(), hash.as_str()] {
            material.extend_from_slice(field.as_bytes());
            material.push(0);
        }
        material.push(b'\n');
    }
    let report = changes
        .iter()
        .filter_map(|change| {
            locked_path(change, &globs, &agent_config).map(|path| LockedChange {
                path: path.to_string(),
                change: change.kind,
            })
        })
        .collect();
    Ok((report, aaf_fp::sha256_hex(&material)))
}

fn valid_approval(data_dir: &Path, repo_root: &Path, lock_fp: &str) -> Option<Approval> {
    // No key means nothing was ever signed here. An unreadable, unsigned or
    // tampered approval file is treated as no approval.
    let key = aaf_fp::load_key(data_dir).ok()?;
    aaf_fp::read_approval(data_dir, &key, repo_root, lock_fp)
        .ok()
        .flatten()
        .filter(|approval| approval.passes(lock_fp))
}

pub(crate) fn evaluate_changes(
    data_dir: &Path,
    repo_root: &Path,
    cfg: &RepoConfig,
    changes: &[Change],
) -> Result<HeadLockReport, GateError> {
    let (locked_changes, lock_fp) = locked(repo_root, cfg, changes)?;
    if locked_changes.is_empty() {
        return Ok(HeadLockReport {
            status: LockStatus::Clear,
            lock_fp,
            locked_changes,
            approval: None,
        });
    }
    let approval = valid_approval(data_dir, repo_root, &lock_fp);
    Ok(HeadLockReport {
        status: if approval.is_some() {
            LockStatus::Approved
        } else {
            LockStatus::Blocked
        },
        lock_fp,
        locked_changes,
        approval,
    })
}

/// Head-lock state of the work tree against `base_sha`. Fails closed: when
/// the change set cannot be computed the report is `Blocked` with no
/// fingerprint, so nothing can approve it.
pub fn evaluate(
    data_dir: &Path,
    repo_root: &Path,
    cfg: &RepoConfig,
    base_sha: &str,
) -> HeadLockReport {
    changes::collect(repo_root, base_sha, GIT_TIMEOUT)
        .and_then(|set| evaluate_changes(data_dir, repo_root, cfg, &set.changes))
        .unwrap_or_else(|_| HeadLockReport {
            status: LockStatus::Blocked,
            lock_fp: String::new(),
            locked_changes: Vec::new(),
            approval: None,
        })
}

/// Signs a `PASS` approval for the locked changes as they are right now.
pub fn approve(
    data_dir: &Path,
    repo_root: &Path,
    cfg: &RepoConfig,
    reviewer: &str,
    same_account: bool,
    clock: &dyn Clock,
) -> Result<Approval, GateError> {
    let base = base::resolve(repo_root, &cfg.gate.base_ref, GIT_TIMEOUT)?;
    let set = changes::collect(repo_root, &base.diff_target, GIT_TIMEOUT)?;
    let (locked_changes, lock_fp) = locked(repo_root, cfg, &set.changes)?;
    if locked_changes.is_empty() {
        return Err(GateError::NothingLocked);
    }
    let approval = Approval {
        lock_fp,
        reviewer: reviewer.to_string(),
        verdict: "PASS".to_string(),
        same_account,
        created_at: clock.now(),
    };
    let key = aaf_fp::load_or_create_key(data_dir)?;
    aaf_fp::write_approval(data_dir, &key, repo_root, &approval)?;
    Ok(approval)
}
