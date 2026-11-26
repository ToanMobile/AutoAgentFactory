//! Head lock: existing tests, CI and build files may not change without a
//! signed approval of that exact set of changes.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use aaf_domain::config::{RepoConfig, TEST_FILE_GLOBS};
use aaf_domain::gate::{Approval, ChangeKind, HeadLockReport, LockStatus, LockTier, LockedChange};
use aaf_domain::ports::Clock;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use super::changes::{self, AddedLines, Change};
use super::{GIT_TIMEOUT, GateError, base, static_checks};

mod dir;

fn glob_set(globs: &[String], case_insensitive: bool) -> Result<GlobSet, GateError> {
    let mut builder = GlobSetBuilder::new();
    for glob in globs {
        let glob = GlobBuilder::new(glob)
            .literal_separator(true)
            .case_insensitive(case_insensitive)
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
pub(super) fn content_hash(repo_root: &Path, path: &str) -> Result<String, GateError> {
    let trimmed = path.trim_end_matches('/');
    let full = repo_root.join(trimmed);
    let Ok(meta) = fs::symlink_metadata(&full) else {
        // A name git reported in non-UTF-8 bytes was decoded lossily, so
        // the decoded name is never on disk: it is not a deletion an
        // approval could cover.
        if path.contains('\u{FFFD}') {
            return Err(GateError::Config(format!(
                "locked path {path} is not valid UTF-8"
            )));
        }
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
    if meta.is_dir() && !trimmed.is_empty() {
        return dir::hash_dir(&full, path);
    }
    Err(GateError::Config(format!(
        "locked path {path} is neither a file nor a symlink"
    )))
}

struct LockMatchers {
    /// Configured test-file globs (case-sensitive so `*Test.kt` does not match `latest.kt`).
    test_globs: GlobSet,
    /// Non-test configured globs, matched case-insensitively so APFS case
    /// variants (`cargo.toml`, `.Cargo/config.toml`, `claude.md`, `tests/Conftest.py`)
    /// cannot bypass head-lock.
    non_test_globs: GlobSet,
    /// Case-insensitive directory prefixes of `non_test_globs` to catch symlinked
    /// directories such as `ln -s helper .cargo`.
    symlink_dir_globs: GlobSet,
}

impl LockMatchers {
    fn from_globs(globs: &[String], extra_hard_globs: &[String]) -> Result<Self, GateError> {
        let mut test = Vec::new();
        let mut non_test = Vec::new();
        for g in globs {
            if TEST_FILE_GLOBS.contains(&g.as_str()) {
                test.push(g.clone());
            } else {
                non_test.push(g.clone());
            }
        }
        for g in extra_hard_globs {
            if !non_test.contains(g) {
                non_test.push(g.clone());
            }
        }
        let dir_prefixes = changes::symlink_dir_prefixes(&non_test);
        Ok(Self {
            test_globs: glob_set(&test, false)?,
            non_test_globs: glob_set(&non_test, true)?,
            symlink_dir_globs: glob_set(&dir_prefixes, true)?,
        })
    }

    fn matches_any(&self, path: &str) -> bool {
        self.test_globs.is_match(path) || self.non_test_globs.is_match(path)
    }

    fn matches_dir_prefix(&self, path: &str) -> bool {
        self.symlink_dir_globs.is_match(path.trim_end_matches('/'))
    }

    fn matches_symlink_dir(&self, repo_root: &Path, path: &str) -> bool {
        changes::matches_locked_dir(&self.symlink_dir_globs, repo_root, path)
    }

    /// Tier-2 (`Soft`) ONLY for pure test-file paths that match no Tier-1 glob,
    /// no locked symlink directory prefix, and no inline `aaf:allow(...)` path.
    fn classify_tier(
        &self,
        locked_path: &str,
        current_path: &str,
        allow_paths: &HashSet<&str>,
    ) -> LockTier {
        let is_pure_test = self.test_globs.is_match(locked_path)
            && !self.non_test_globs.is_match(locked_path)
            && !self.non_test_globs.is_match(current_path)
            && !self.matches_dir_prefix(locked_path)
            && !self.matches_dir_prefix(current_path)
            && !allow_paths.contains(locked_path)
            && !allow_paths.contains(current_path);
        if is_pure_test {
            LockTier::Soft
        } else {
            LockTier::Hard
        }
    }
}

/// The path `change` is locked under, if it is locked.
///
/// **Added**: exempt when the new file does not match any non-test locked glob
/// (case-insensitive) and is not a symlink shadowing a locked config directory.
///
/// **Renamed**: checks `old_path` against all locked globs and locked symlink
/// directory prefixes (blocking moving a locked file or config symlink out),
/// and when `old_path` is unlocked checks `path` against non-test locked globs
/// and locked symlink directories (blocking renaming a benign file into a
/// locked config while allowing unlocked -> new test file).
///
/// **Modified / Deleted**: judged by the file path against all locked globs
/// (plus locked symlink directories, or symlink-dir prefixes when deleted).
fn locked_path<'a>(
    repo_root: &Path,
    change: &'a Change,
    matchers: &LockMatchers,
) -> Option<&'a str> {
    match change.kind {
        ChangeKind::Added => (matchers.non_test_globs.is_match(&change.path)
            || matchers.matches_symlink_dir(repo_root, &change.path))
        .then_some(change.path.as_str()),
        ChangeKind::Renamed => {
            let old = change.old_path.as_deref().unwrap_or(&change.path);
            if matchers.matches_any(old) || matchers.matches_dir_prefix(old) {
                Some(old)
            } else if matchers.non_test_globs.is_match(&change.path)
                || matchers.matches_symlink_dir(repo_root, &change.path)
            {
                Some(change.path.as_str())
            } else {
                None
            }
        }
        ChangeKind::Modified | ChangeKind::Deleted => {
            let path = change.old_path.as_deref().unwrap_or(&change.path);
            let symlink_dir = if change.kind == ChangeKind::Deleted {
                matchers.matches_dir_prefix(path)
            } else {
                matchers.matches_symlink_dir(repo_root, path)
            };
            (matchers.matches_any(path) || symlink_dir).then_some(path)
        }
    }
}

type LockEntry = (String, &'static str, String, String);

/// Locked changes, their fingerprint, and exact sorted entry tuples, with
/// the content of each changed path read by `hash` (callers memoise it so
/// derived views (base config, tracked-only changes) reuse the exact same
/// single working-tree read).
fn locked_with(
    repo_root: &Path,
    cfg: &RepoConfig,
    extra_hard_globs: &[String],
    changes: &[Change],
    added: &[AddedLines],
    hash: &mut dyn FnMut(&str) -> Result<String, GateError>,
) -> Result<(Vec<LockedChange>, String, Vec<LockEntry>), GateError> {
    let matchers = LockMatchers::from_globs(&cfg.gate.head_lock_globs, extra_hard_globs)?;
    let mut allow_paths: HashSet<&str> = HashSet::new();
    for file in added {
        for (_, text) in &file.lines {
            if static_checks::contains_allow_marker(text)? {
                allow_paths.insert(file.path.as_str());
                break;
            }
        }
    }
    let mut entries: Vec<LockEntry> = Vec::new();
    let mut report: Vec<LockedChange> = Vec::new();
    for change in changes {
        let Some(path) = locked_path(repo_root, change, &matchers).or_else(|| {
            allow_paths
                .contains(change.path.as_str())
                .then_some(change.path.as_str())
        }) else {
            continue;
        };
        // The hash follows the content on disk, so an approval stops
        // covering the change the moment the file is edited again.
        let hash = hash(&change.path)?;
        // A directory's path says nothing of the files inside it: never Soft.
        let tier = if hash.starts_with("dir:") {
            LockTier::Hard
        } else {
            matchers.classify_tier(path, &change.path, &allow_paths)
        };
        entries.push((
            path.to_string(),
            change.kind.letter(),
            change.path.clone(),
            hash,
        ));
        report.push(LockedChange {
            path: path.to_string(),
            change: change.kind,
            tier,
        });
    }
    if entries.is_empty() {
        return Ok((Vec::new(), String::new(), Vec::new()));
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
    Ok((report, aaf_fp::sha256_hex(&material), entries))
}

pub(crate) use crate::confine_core::git_refs::common_repo_root;

fn valid_approval(
    data_dir: &Path,
    repo_root: &Path,
    common_root: Option<&Path>,
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
    match aaf_fp::read_approval(data_dir, &key, repo_root, lock_fp) {
        Ok(Some(approval)) if check(&approval) => return Some(approval),
        Ok(_) => {}
        Err(_) => return None,
    }
    // Also accept a content-bound approval signed in the repository's primary
    // checkout (e.g. when `aaf merge` verifies in a detached scratch worktree).
    let resolved_common;
    let common_root = match common_root {
        Some(path) => Some(path),
        None => {
            resolved_common = common_repo_root(repo_root);
            resolved_common.as_deref()
        }
    };
    if let Some(common_root) = common_root
        && common_root != repo_root
        && let Ok(Some(approval)) = aaf_fp::read_approval(data_dir, &key, common_root, lock_fp)
    {
        return check(&approval).then_some(approval);
    }
    None
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn evaluate_changes_with_common(
    data_dir: &Path,
    repo_root: &Path,
    common_root: Option<&Path>,
    cfg: &RepoConfig,
    extra_hard_globs: &[String],
    changes: &[Change],
    added: &[AddedLines],
    require_independent: bool,
) -> Result<HeadLockReport, GateError> {
    let effective =
        changes::with_ignored_matching(repo_root, extra_hard_globs, changes, GIT_TIMEOUT)?;
    let mut hash = |path: &str| content_hash(repo_root, path);
    let (locked_changes, lock_fp, _) = locked_with(
        repo_root,
        cfg,
        extra_hard_globs,
        &effective,
        added,
        &mut hash,
    )?;
    if locked_changes.is_empty() {
        return Ok(HeadLockReport {
            status: LockStatus::Clear,
            lock_fp,
            locked_changes,
            approval: None,
        });
    }
    let has_hard_lock = locked_changes.iter().any(|c| c.tier == LockTier::Hard);
    let need_independent =
        require_independent || cfg.cross_audit.require_different_provider || has_hard_lock;
    let approval = valid_approval(data_dir, repo_root, common_root, &lock_fp, need_independent);
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
        .and_then(|set| {
            let added = changes::added_lines(
                repo_root,
                base_sha,
                &set.untracked,
                &static_checks::skip_path,
                GIT_TIMEOUT,
            )?;
            let (_, extra_hard) =
                crate::proof::load_gate_base_capture_cmd(repo_root, Some(base_sha))
                    .map_err(|e| GateError::Config(e.to_string()))?;
            let req = cfg.cross_audit.require_different_provider;
            evaluate_changes_with_common(
                data_dir,
                repo_root,
                None,
                cfg,
                &extra_hard,
                &set.changes,
                &added,
                req,
            )
        })
        .unwrap_or_else(|_| HeadLockReport {
            status: LockStatus::Blocked,
            lock_fp: String::new(),
            locked_changes: Vec::new(),
            approval: None,
        })
}

pub(crate) fn write_approval_everywhere(
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
    let who = (reviewer, same_account);
    let mut hash = |path: &str| content_hash(repo_root, path);
    approve_inner(data_dir, repo_root, cfg, &base, who, clock, None, &mut hash)
}

/// Signs a `PASS` approval only if the locked-file fingerprint still matches
/// `expected_lock_fp` (closing TOCTOU between preview and confirmation).
pub fn approve_matching(
    data_dir: &Path,
    repo_root: &Path,
    cfg: &RepoConfig,
    reviewer: &str,
    same_account: bool,
    expected_lock_fp: &str,
    clock: &dyn Clock,
) -> Result<Approval, GateError> {
    let base = base::resolve(repo_root, &cfg.gate.base_ref, GIT_TIMEOUT)?;
    approve_expecting(
        data_dir,
        repo_root,
        cfg,
        &base,
        (reviewer, same_account),
        clock,
        expected_lock_fp,
    )
}

/// [`approve`] against an already resolved `base`, for the locked diff a
/// reviewer was shown only: `NothingLocked` when no locked file is changed
/// and `LockFpChanged` when their `lock_fp` is no longer `expected`, both
/// decided on one evaluation and before the key is loaded or anything is
/// written (audit a13 L8-R3-1). `who` is (reviewer, same account).
pub fn approve_expecting(
    data_dir: &Path,
    repo_root: &Path,
    cfg: &RepoConfig,
    base: &base::Base,
    who: (&str, bool),
    clock: &dyn Clock,
    expected: &str,
) -> Result<Approval, GateError> {
    let mut hash = |path: &str| content_hash(repo_root, path);
    let expected = Some(expected);
    approve_inner(
        data_dir, repo_root, cfg, base, who, clock, expected, &mut hash,
    )
}

/// The approvals for the locked changes, every fingerprint from the content
/// `read_hash` gave for each path, read once per path. With `expected`, see
/// [`approve_expecting`] (`NothingLocked` / `LockFpChanged`).
#[allow(clippy::too_many_arguments)]
pub(super) fn approve_inner(
    data_dir: &Path,
    repo_root: &Path,
    cfg: &RepoConfig,
    base: &base::Base,
    (reviewer, same_account): (&str, bool),
    clock: &dyn Clock,
    expected_lock_fp: Option<&str>,
    read_hash: &mut dyn FnMut(&str) -> Result<String, GateError>,
) -> Result<Approval, GateError> {
    let set = changes::collect(repo_root, &base.diff_target, GIT_TIMEOUT)?;
    let added = changes::added_lines(
        repo_root,
        &base.diff_target,
        &set.untracked,
        &static_checks::skip_path,
        GIT_TIMEOUT,
    )?;
    // One read per path for every fingerprint signed below: a file edited
    // while this runs (after the `expected` check, during the `git show`)
    // must not reach any approval (audit a13 R4-L2-1).
    let mut seen_content = HashMap::<String, String>::new();
    let mut hash = |path: &str| -> Result<String, GateError> {
        if let Some(hash) = seen_content.get(path) {
            return Ok(hash.clone());
        }
        let hash = read_hash(path)?;
        seen_content.insert(path.to_string(), hash.clone());
        Ok(hash)
    };
    let (_, extra_hard) = crate::proof::load_gate_base_capture_cmd(repo_root, base.sha.as_deref())
        .map_err(|e| GateError::Config(e.to_string()))?;
    let working_changes =
        changes::with_ignored_matching(repo_root, &extra_hard, &set.changes, GIT_TIMEOUT)?;
    let (locked_changes, lock_fp, primary_entries) = locked_with(
        repo_root,
        cfg,
        &extra_hard,
        &working_changes,
        &added,
        &mut hash,
    )?;
    if locked_changes.is_empty() && expected_lock_fp.is_some() {
        return Err(GateError::NothingLocked);
    }
    if !locked_changes.is_empty()
        && let Some(expected) = expected_lock_fp
        && lock_fp != expected
    {
        return Err(GateError::LockFpChanged);
    }
    // `aaf merge` evaluates head-lock against the trusted config on `base` inside
    // a clean detached scratch worktree (no untracked local worktree files such
    // as `.agents/` or `.claude/`). Sign the exact content-bound fingerprints for
    // those views as well — only when every locked entry in the derived view was
    // already part of the primary set the reviewer previewed.
    let base_cfg = base.sha.as_deref().and_then(|sha| {
        let spec = format!("{sha}:.autofactory/config.toml");
        let raw = aaf_fp::git(repo_root, &["show", &spec], &[], GIT_TIMEOUT).ok()?;
        aaf_domain::builtin_defaults::parse_repo(&raw).ok()
    });
    let base_view = base_cfg.as_ref().and_then(|b| {
        let (bl, fp, ent) = locked_with(
            repo_root,
            b,
            &extra_hard,
            &working_changes,
            &added,
            &mut hash,
        )
        .ok()?;
        (!bl.is_empty()).then_some((fp, ent))
    });
    let (primary_fp, allowed_entries, base_view) = if !locked_changes.is_empty() {
        (lock_fp, primary_entries, base_view)
    } else if let Some((fp, base_entries)) = base_view {
        (fp, base_entries, None)
    } else {
        return Err(GateError::NothingLocked);
    };
    let primary_set: HashSet<&LockEntry> = allowed_entries.iter().collect();
    let tracked_changes: Vec<Change> = if set.untracked.is_empty() {
        Vec::new()
    } else {
        let untracked_set: HashSet<&str> = set.untracked.iter().map(String::as_str).collect();
        set.changes
            .iter()
            .filter(|c| !(c.kind == ChangeKind::Added && untracked_set.contains(c.path.as_str())))
            .cloned()
            .collect()
    };
    let mut extra_fps: Vec<String> = Vec::new();
    if let Some((fp, base_entries)) = base_view
        && base_entries.iter().all(|e| primary_set.contains(e))
    {
        extra_fps.push(fp);
    }
    if !tracked_changes.is_empty() {
        for c in [Some(cfg), base_cfg.as_ref()].into_iter().flatten() {
            if let Ok((tl, fp, ent)) = locked_with(
                repo_root,
                c,
                &extra_hard,
                &tracked_changes,
                &added,
                &mut hash,
            ) && !tl.is_empty()
                && ent.iter().all(|e| primary_set.contains(e))
            {
                extra_fps.push(fp);
            }
        }
    }
    let approval = Approval {
        lock_fp: primary_fp,
        reviewer: reviewer.to_string(),
        verdict: "PASS".to_string(),
        same_account,
        created_at: clock.now(),
    };
    let key = aaf_fp::load_or_create_key(data_dir)?;
    write_approval_everywhere(data_dir, &key, repo_root, &approval)?;
    let mut seen = HashSet::from([approval.lock_fp.clone()]);
    for extra_fp in extra_fps {
        if seen.insert(extra_fp.clone()) {
            let extra = Approval {
                lock_fp: extra_fp,
                ..approval.clone()
            };
            write_approval_everywhere(data_dir, &key, repo_root, &extra)?;
        }
    }
    Ok(approval)
}
