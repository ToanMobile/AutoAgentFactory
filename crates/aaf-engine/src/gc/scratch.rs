//! Fail-closed sweeping of merge-scratch worktrees, detached oracle worktrees
//! (`aaf-oracle-wt-*` and legacy `<data_dir>/oracle-scratch/*`), and temporary
//! `refs/aaf-merge/*` refs.
//!
//! All liveness checks use [`crate::process::process_alive`] (fail-closed) and
//! never follow symlinks, never delete locked or `cwd`-containing trees, and
//! never create `.merge.lock` during `--dry-run`.

use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::expired::{is_older_than, sorted_real_dir_entries};
use super::{GcOptions, GcReport, check_cwd_blocks, record_removed_file, resolved_path};
use crate::gate::oracle_worktree::ORACLE_SCRATCH_DIR;
use crate::git::cmd::{git, git_raw};
use crate::git::worktree::WorktreeEntry;
use crate::process::process_alive;
use crate::telemetry::is_exclusively_locked;

/// Minimum age for legacy `oracle-scratch` dirs and legacy non-PID `refs/aaf-merge/pr-{N}`.
const LEGACY_MIN_AGE: Duration = Duration::from_secs(3600);
/// Bound on the `.git` / `gitdir` pointer files read for legacy `oracle-scratch`.
const MAX_POINTER_BYTES: u64 = 4096;

fn is_ascii_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn is_hex16(s: &str) -> bool {
    s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Parses `<common_id>-pr<N>-<pid>-<seq>` and validates `common_id == expected_common_id`.
fn parse_repo_merge_scratch_pid(name: &str, expected_common_id: &str) -> Option<u32> {
    if !is_hex16(expected_common_id) {
        return None;
    }
    let rest = name.strip_prefix(expected_common_id)?.strip_prefix("-pr")?;
    let mut parts = rest.split('-');
    let pr_num = parts.next()?;
    let pid_str = parts.next()?;
    let seq = parts.next()?;
    if parts.next().is_some() || !is_ascii_digits(pr_num) || !is_ascii_digits(seq) {
        return None;
    }
    pid_str.parse::<u32>().ok().filter(|&p| p > 1)
}

/// Parses `aaf-oracle-wt-<pid>-<seq>-<nanos>`.
pub(crate) fn parse_oracle_wt_pid(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("aaf-oracle-wt-")?;
    let mut parts = rest.split('-');
    let pid_str = parts.next()?;
    let seq = parts.next()?;
    let nanos = parts.next()?;
    if parts.next().is_some() || !is_ascii_digits(seq) || !is_ascii_digits(nanos) {
        return None;
    }
    pid_str.parse::<u32>().ok().filter(|&p| p > 1)
}

enum MergeRefKind {
    PidTagged(u32),
    Legacy,
    Unknown,
}

fn classify_merge_ref(refname: &str) -> MergeRefKind {
    let Some(rest) = refname.strip_prefix("refs/aaf-merge/pr-") else {
        return MergeRefKind::Unknown;
    };
    if is_ascii_digits(rest) {
        return MergeRefKind::Legacy;
    }
    let mut parts = rest.split('-');
    let (Some(pr_num), Some(pid_str), Some(seq), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return MergeRefKind::Unknown;
    };
    if !is_ascii_digits(pr_num) || !is_ascii_digits(seq) {
        return MergeRefKind::Unknown;
    }
    match pid_str.parse::<u32>().ok().filter(|&p| p > 1) {
        Some(pid) => MergeRefKind::PidTagged(pid),
        None => MergeRefKind::Unknown,
    }
}

/// Fail-closed check whether `<locks>/<common_id>.merge.lock` is held without
/// ever creating the file.
fn merge_lock_is_busy(lock_path: &Path) -> bool {
    let Ok(meta) = fs::symlink_metadata(lock_path) else {
        return false;
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        return true;
    }
    let Ok(file) = File::open(lock_path) else {
        return true;
    };
    match file.try_lock_shared() {
        Ok(()) => {
            let _ = file.unlock();
            false
        }
        Err(TryLockError::WouldBlock | TryLockError::Error(_)) => true,
    }
}

fn ref_mtime_older_than(
    common_dir: &Path,
    refname: &str,
    min_age: Duration,
    now: SystemTime,
) -> bool {
    let loose = common_dir.join(refname);
    let meta = match fs::symlink_metadata(&loose) {
        Ok(m) if !m.file_type().is_symlink() && m.is_file() => Some(m),
        _ => fs::symlink_metadata(common_dir.join("packed-refs"))
            .ok()
            .filter(|m| !m.file_type().is_symlink() && m.is_file()),
    };
    let Some(meta) = meta else {
        return false;
    };
    meta.modified()
        .ok()
        .and_then(|m| now.duration_since(m).ok())
        .is_some_and(|age| age > min_age)
}

/// Sweeps repo-scoped dead-PID merge-scratch worktrees, detached
/// `aaf-oracle-wt-*` worktrees, and stale `refs/aaf-merge/*` refs.
#[allow(clippy::too_many_arguments)]
pub(super) fn clean_repo_scratch(
    repo: &Path,
    data_dir: &Path,
    registered: &[WorktreeEntry],
    merge_refs: &[String],
    cwd: Option<&Path>,
    dry_run: bool,
    now: SystemTime,
    report: &mut GcReport,
) {
    clean_repo_oracle_wt(repo, registered, cwd, dry_run, report);
    let has_merge_scratch = data_dir.join("merge-scratch").is_dir();
    if !has_merge_scratch && merge_refs.is_empty() {
        return;
    }
    let common_id = crate::gate::stats_repo_id(repo);
    if has_merge_scratch {
        clean_repo_merge_scratch(repo, data_dir, &common_id, registered, cwd, dry_run, report);
    }
    clean_repo_merge_refs(repo, data_dir, &common_id, merge_refs, dry_run, now, report);
}

fn clean_repo_merge_scratch(
    repo: &Path,
    data_dir: &Path,
    common_id: &str,
    registered: &[WorktreeEntry],
    cwd: Option<&Path>,
    dry_run: bool,
    report: &mut GcReport,
) {
    if !is_hex16(common_id) {
        return;
    }
    let merge_lock = data_dir
        .join("locks")
        .join(format!("{common_id}.merge.lock"));
    for path in sorted_real_dir_entries(&data_dir.join("merge-scratch")) {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(pid) = parse_repo_merge_scratch_pid(name, common_id) else {
            continue;
        };
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_dir() || meta.file_type().is_symlink() {
            continue;
        }
        let mut lock_os = path.as_os_str().to_os_string();
        lock_os.push(".lock");
        let companion_lock = PathBuf::from(lock_os);
        if is_exclusively_locked(&companion_lock) || merge_lock_is_busy(&merge_lock) {
            continue;
        }
        if process_alive(pid) {
            continue;
        }
        let res_path = resolved_path(&path);
        let reg_entry = registered
            .iter()
            .find(|e| resolved_path(&e.path) == res_path);
        if reg_entry.is_some_and(|e| e.locked) {
            report.warnings.push(format!(
                "{}: merge-scratch worktree is locked: kept",
                path.display()
            ));
            continue;
        }
        if let Some(reason) = check_cwd_blocks(cwd, &path) {
            report
                .warnings
                .push(format!("{}: {reason}: kept", path.display()));
            continue;
        }
        if dry_run {
            record_removed_file(report, &path);
            continue;
        }
        if (reg_entry.is_some() || path.join(".git").exists())
            && let Some(p) = path.to_str()
        {
            let _ = git_raw(repo, &["worktree", "remove", "--force", p]);
        }
        if fs::symlink_metadata(&path).is_ok()
            && let Err(err) = fs::remove_dir_all(&path)
        {
            report
                .warnings
                .push(format!("{}: merge-scratch remove: {err}", path.display()));
            continue;
        }
        if !path.exists()
            && fs::symlink_metadata(&companion_lock)
                .is_ok_and(|m| m.is_file() && !m.file_type().is_symlink())
        {
            let _ = fs::remove_file(&companion_lock);
        }
        record_removed_file(report, &path);
    }
}

fn clean_repo_oracle_wt(
    repo: &Path,
    registered: &[WorktreeEntry],
    cwd: Option<&Path>,
    dry_run: bool,
    report: &mut GcReport,
) {
    for (idx, entry) in registered.iter().enumerate() {
        if idx == 0 || entry.branch.is_some() {
            continue;
        }
        let Some(name) = entry.path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(pid) = parse_oracle_wt_pid(name) else {
            continue;
        };
        if process_alive(pid) {
            continue;
        }
        if entry.locked {
            report.warnings.push(format!(
                "{}: oracle worktree is locked: kept",
                entry.path.display()
            ));
            continue;
        }
        if let Ok(meta) = fs::symlink_metadata(&entry.path) {
            if meta.file_type().is_symlink() {
                report.warnings.push(format!(
                    "{}: oracle worktree path is a symlink: kept",
                    entry.path.display()
                ));
                continue;
            }
            if let Some(reason) = check_cwd_blocks(cwd, &entry.path) {
                report
                    .warnings
                    .push(format!("{}: {reason}: kept", entry.path.display()));
                continue;
            }
        } else if cwd.is_some_and(|c| fs::canonicalize(c).is_err()) {
            report.warnings.push(format!(
                "{}: cannot canonicalize cwd: kept",
                entry.path.display()
            ));
            continue;
        }
        if dry_run {
            record_removed_file(report, &entry.path);
            continue;
        }
        if let Some(p) = entry.path.to_str() {
            let _ = git_raw(repo, &["worktree", "remove", "--force", p]);
        }
        if fs::symlink_metadata(&entry.path)
            .is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
            && let Err(err) = fs::remove_dir_all(&entry.path)
        {
            report.warnings.push(format!(
                "{}: oracle worktree remove: {err}",
                entry.path.display()
            ));
            continue;
        }
        record_removed_file(report, &entry.path);
    }
}

fn clean_repo_merge_refs(
    repo: &Path,
    data_dir: &Path,
    common_id: &str,
    merge_refs: &[String],
    dry_run: bool,
    now: SystemTime,
    report: &mut GcReport,
) {
    if merge_refs.is_empty() {
        return;
    }
    let common_dir = git(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .ok()
    .map(|s| PathBuf::from(s.trim()));
    let merge_lock = data_dir
        .join("locks")
        .join(format!("{common_id}.merge.lock"));

    for refname in merge_refs {
        let removable = match classify_merge_ref(refname) {
            MergeRefKind::PidTagged(pid) => !process_alive(pid),
            MergeRefKind::Legacy => {
                !merge_lock_is_busy(&merge_lock)
                    && common_dir
                        .as_deref()
                        .is_some_and(|dir| ref_mtime_older_than(dir, refname, LEGACY_MIN_AGE, now))
            }
            MergeRefKind::Unknown => false,
        };
        if !removable {
            continue;
        }
        if !dry_run {
            match git_raw(repo, &["update-ref", "-d", refname]) {
                Ok(out) if out.success() => {}
                Ok(out) => {
                    report
                        .warnings
                        .push(format!("{refname}: update-ref -d: {}", out.stderr.trim()));
                    continue;
                }
                Err(err) => {
                    report
                        .warnings
                        .push(format!("{refname}: update-ref -d: {err}"));
                    continue;
                }
            }
        }
        report.removed_merge_refs += 1;
    }
}

/// Sweeps legacy `<data_dir>/oracle-scratch/<common_id>-<pid>-<seq>` dirs whose
/// owning gate died before cleanup.
pub(super) fn clean_legacy_oracle_scratch(
    data_dir: &Path,
    opts: &GcOptions,
    now: SystemTime,
    report: &mut GcReport,
) {
    let max_age =
        Duration::from_secs(opts.older_than_days.saturating_mul(86_400)).min(LEGACY_MIN_AGE);
    for path in sorted_real_dir_entries(&data_dir.join(ORACLE_SCRATCH_DIR)) {
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_dir() || meta.file_type().is_symlink() || !is_older_than(&meta, max_age, now) {
            continue;
        }
        let Some(pid) = legacy_oracle_owner_pid(&path) else {
            continue;
        };
        if process_alive(pid) {
            continue;
        }
        if let Some(reason) = check_cwd_blocks(opts.cwd.as_deref(), &path) {
            report
                .warnings
                .push(format!("{}: {reason}: kept", path.display()));
            continue;
        }
        if opts.dry_run {
            record_removed_file(report, &path);
        } else {
            remove_legacy_oracle_scratch(&path, report);
        }
    }
}

fn legacy_oracle_owner_pid(path: &Path) -> Option<u32> {
    let name = path.file_name()?.to_str()?;
    let mut parts = name.rsplitn(3, '-');
    let seq = parts.next()?;
    let pid = parts.next()?;
    let id = parts.next()?;
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric()) || !is_ascii_digits(seq) {
        return None;
    }
    pid.parse::<u32>().ok().filter(|&p| p > 1)
}

fn read_pointer(path: &Path) -> Option<String> {
    let meta = fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_POINTER_BYTES {
        return None;
    }
    fs::read_to_string(path).ok()
}

fn registration(scratch: &Path) -> Option<PathBuf> {
    let gitfile = scratch.join(".git");
    let text = read_pointer(&gitfile)?;
    let admin = fs::canonicalize(scratch.join(text.trim().strip_prefix("gitdir:")?.trim())).ok()?;
    if admin.parent()?.file_name()? != "worktrees" {
        return None;
    }
    let back = read_pointer(&admin.join("gitdir"))?;
    let back = fs::canonicalize(admin.join(back.trim())).ok()?;
    (back == fs::canonicalize(&gitfile).ok()?).then_some(admin)
}

fn remove_legacy_oracle_scratch(path: &Path, report: &mut GcReport) {
    let admin = registration(path);
    if admin.as_ref().is_some_and(|a| a.join("locked").exists()) {
        report.warnings.push(format!(
            "{}: oracle scratch is locked; kept",
            path.display()
        ));
        return;
    }
    let git_cwd = admin
        .as_deref()
        .and_then(|a| a.parent()?.parent())
        .map(|common| match common.parent() {
            Some(root) if common.file_name().is_some_and(|n| n == ".git") => root.to_path_buf(),
            _ => common.to_path_buf(),
        });
    if let (Some(cwd), Some(p)) = (&git_cwd, path.to_str()) {
        let _ = git_raw(cwd, &["worktree", "remove", "--force", p]);
    }
    if fs::symlink_metadata(path).is_ok() {
        if let Err(err) = fs::remove_dir_all(path) {
            report.warnings.push(format!("{}: {err}", path.display()));
            return;
        }
        if let (Some(cwd), Some(a)) = (&git_cwd, &admin)
            && a.exists()
        {
            let _ = git_raw(cwd, &["worktree", "prune", "--expire=5.minutes.ago"]);
        }
    }
    record_removed_file(report, path);
}
