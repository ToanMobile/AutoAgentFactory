//! Sweep of red->green oracle scratch worktrees
//! (`<data_dir>/oracle-scratch/<common_id>-<pid>-<seq>`) whose gate died
//! before its RAII guard ran (SIGKILL, power loss, a terminal closed).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::expired::{is_older_than, sorted_real_dir_entries};
use super::{GcOptions, GcReport};
use crate::gate::oracle_worktree::ORACLE_SCRATCH_DIR;
use crate::git::cmd::git_raw;

/// A dead owner's scratch younger than this is kept. Age is checked before
/// the owner probe, as for merge-scratch locks; `--older-than` shorter than
/// this still wins.
const YOUNG: Duration = Duration::from_secs(3600);
/// Bound on the `.git` / `gitdir` pointer files gc reads.
const MAX_POINTER_BYTES: u64 = 4096;

pub(super) fn clean_oracle_scratch(
    data_dir: &Path,
    opts: &GcOptions,
    now: SystemTime,
    report: &mut GcReport,
) {
    let max_age = Duration::from_secs(opts.older_than_days.saturating_mul(86_400)).min(YOUNG);
    for path in sorted_real_dir_entries(&data_dir.join(ORACLE_SCRATCH_DIR)) {
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_dir() || meta.file_type().is_symlink() || !is_older_than(&meta, max_age, now) {
            continue;
        }
        let Some(pid) = owner_pid(&path) else {
            continue;
        };
        if crate::executors::reaper::is_alive(pid).unwrap_or(true) {
            continue;
        }
        if opts.dry_run {
            report.removed_files.push(path.display().to_string());
        } else {
            remove_scratch(&path, report);
        }
    }
}

/// Owner pid encoded in `<common_id>-<pid>-<seq>`.
fn owner_pid(path: &Path) -> Option<u32> {
    let name = path.file_name()?.to_str()?;
    let mut parts = name.rsplitn(3, '-');
    let seq = parts.next()?;
    let pid = parts.next()?;
    let id = parts.next()?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric()) || !digits(seq) {
        return None;
    }
    pid.parse::<u32>().ok().filter(|p| *p > 1)
}

fn read_pointer(path: &Path) -> Option<String> {
    let meta = fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_POINTER_BYTES {
        return None;
    }
    fs::read_to_string(path).ok()
}

/// The admin dir `<common>/worktrees/<name>` registered for `scratch`, only
/// when its `gitdir` file points back at `scratch/.git`: a `.git` file naming
/// another worktree's admin dir never makes gc run git against it.
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

/// `git worktree remove --force` through the owning repository; when that
/// leaves the dir, remove it and prune the stale registration.
fn remove_scratch(path: &Path, report: &mut GcReport) {
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
            let _ = git_raw(cwd, &["worktree", "prune"]);
        }
    }
    report.removed_files.push(path.display().to_string());
}
