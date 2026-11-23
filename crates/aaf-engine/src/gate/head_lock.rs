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

fn common_repo_root(repo_root: &Path) -> Option<std::path::PathBuf> {
    let args = ["rev-parse", "--path-format=absolute", "--git-common-dir"];
    let common = aaf_fp::git(repo_root, &args, &[], GIT_TIMEOUT).ok()?;
    let common = Path::new(&common);
    let main_root = match common.parent() {
        Some(parent) if common.file_name().is_some_and(|name| name == ".git") => parent,
        _ => common,
    };
    Some(main_root.to_path_buf())
}

fn valid_approval(
    data_dir: &Path,
    repo_root: &Path,
    lock_fp: &str,
    require_independent: bool,
) -> Option<Approval> {
    // No key means nothing was ever signed here. An unreadable, unsigned or
    // tampered approval file is treated as no approval.
    let key = aaf_fp::load_key(data_dir).ok()?;
    let check = |approval: &Approval| {
        if require_independent {
            approval.passes_independent(lock_fp)
        } else {
            approval.passes(lock_fp)
        }
    };
    if let Ok(Some(approval)) = aaf_fp::read_approval(data_dir, &key, repo_root, lock_fp) {
        return check(&approval).then_some(approval);
    }
    // Also accept a content-bound approval signed in the repository's primary
    // checkout (e.g. when `aaf merge` verifies in a detached scratch worktree).
    if let Some(common_root) = common_repo_root(repo_root)
        && common_root != repo_root
        && let Ok(Some(approval)) = aaf_fp::read_approval(data_dir, &key, &common_root, lock_fp)
    {
        return check(&approval).then_some(approval);
    }
    None
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
    let approval = valid_approval(
        data_dir,
        repo_root,
        &lock_fp,
        cfg.cross_audit.require_different_provider,
    );
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

fn write_approval_everywhere(
    data_dir: &Path,
    key: &[u8],
    repo_root: &Path,
    approval: &Approval,
) -> Result<(), GateError> {
    aaf_fp::write_approval(data_dir, key, repo_root, approval)?;
    // Also mirror under the primary repository root so `aaf merge` (running in a
    // detached scratch worktree) finds approvals signed in any feature worktree.
    if let Some(common_root) = common_repo_root(repo_root)
        && common_root != repo_root
    {
        let _ = aaf_fp::write_approval(data_dir, key, &common_root, approval);
    }
    Ok(())
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
    // `aaf merge` evaluates head-lock against the trusted config on `base` inside
    // a clean detached scratch worktree (no untracked local worktree files such
    // as `.agents/` or `.claude/`). Sign the exact content-bound fingerprints for
    // those views as well so a human approval signed in a worktree unlocks
    // `aaf merge` without weakening content binding on any tracked file.
    let base_cfg = base.sha.as_deref().and_then(|sha| {
        let spec = format!("{sha}:.autofactory/config.toml");
        let raw = aaf_fp::git(repo_root, &["show", &spec], &[], GIT_TIMEOUT).ok()?;
        toml::from_str::<RepoConfig>(&raw).ok()
    });
    let base_lock_fp = base_cfg.as_ref().and_then(|bcfg| {
        let (base_locked, fp) = locked(repo_root, bcfg, &set.changes).ok()?;
        (!base_locked.is_empty()).then_some(fp)
    });
    let tracked_changes: Vec<Change> = if set.untracked.is_empty() {
        Vec::new()
    } else {
        let untracked_set: std::collections::HashSet<&str> =
            set.untracked.iter().map(String::as_str).collect();
        set.changes
            .iter()
            .filter(|c| !(c.kind == ChangeKind::Added && untracked_set.contains(c.path.as_str())))
            .cloned()
            .collect()
    };
    let mut extra_fps: Vec<String> = Vec::new();
    if let Some(fp) = base_lock_fp.clone() {
        extra_fps.push(fp);
    }
    if !tracked_changes.is_empty() {
        if let Ok((tl, fp)) = locked(repo_root, cfg, &tracked_changes)
            && !tl.is_empty()
        {
            extra_fps.push(fp);
        }
        if let Some(bcfg) = base_cfg.as_ref()
            && let Ok((tl, fp)) = locked(repo_root, bcfg, &tracked_changes)
            && !tl.is_empty()
        {
            extra_fps.push(fp);
        }
    }
    let primary_fp = if !locked_changes.is_empty() {
        lock_fp
    } else if let Some(fp) = base_lock_fp {
        fp
    } else {
        return Err(GateError::NothingLocked);
    };
    let created_at = clock.now();
    let approval = Approval {
        lock_fp: primary_fp,
        reviewer: reviewer.to_string(),
        verdict: "PASS".to_string(),
        same_account,
        created_at,
    };
    let key = aaf_fp::load_or_create_key(data_dir)?;
    write_approval_everywhere(data_dir, &key, repo_root, &approval)?;
    let mut seen = std::collections::HashSet::new();
    seen.insert(approval.lock_fp.clone());
    for extra_fp in extra_fps {
        if seen.insert(extra_fp.clone()) {
            let extra_approval = Approval {
                lock_fp: extra_fp,
                reviewer: reviewer.to_string(),
                verdict: "PASS".to_string(),
                same_account,
                created_at,
            };
            write_approval_everywhere(data_dir, &key, repo_root, &extra_approval)?;
        }
    }
    Ok(approval)
}
