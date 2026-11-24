//! Expiry sweep of `streams/`, `reports/`, terminal `sops/` checkpoints and
//! dead merge scratch dirs under the data directory.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use super::{GcOptions, GcReport};
use crate::telemetry::is_exclusively_locked;

const MAX_SOP_CHECKPOINT_BYTES: u64 = 16 * 1024 * 1024;

fn remove_and_record(path: &Path, is_dir: bool, dry_run: bool, report: &mut GcReport) {
    if !dry_run {
        let res = if is_dir {
            fs::remove_dir_all(path)
        } else {
            fs::remove_file(path)
        };
        if let Err(err) = res {
            report.warnings.push(format!("{}: {err}", path.display()));
            return;
        }
    }
    report.removed_files.push(path.display().to_string());
}

pub(super) fn clean_expired_artifacts(
    data_dir: &Path,
    opts: &GcOptions,
    running_runs: &HashSet<String>,
    now: SystemTime,
    report: &mut GcReport,
) {
    let max_age = Duration::from_secs(opts.older_than_days.saturating_mul(86_400));
    for subdir in ["streams", "reports"] {
        for path in sorted_real_dir_entries(&data_dir.join(subdir)) {
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            let ext = path.extension().and_then(|e| e.to_str());
            if !meta.is_file()
                || meta.file_type().is_symlink()
                || !matches!(ext, Some("jsonl" | "html"))
            {
                continue;
            }
            if ext == Some("jsonl")
                && path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| running_runs.contains(s))
            {
                continue;
            }
            if is_older_than(&meta, max_age, now) {
                remove_and_record(&path, false, opts.dry_run, report);
            }
        }
    }
    for path in sorted_real_dir_entries(&data_dir.join("sops")) {
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_file()
            && !meta.file_type().is_symlink()
            && path.extension().and_then(|e| e.to_str()) == Some("json")
            && is_older_than(&meta, max_age, now)
            && is_terminal_sop_file(&path)
        {
            remove_and_record(&path, false, opts.dry_run, report);
        }
    }
    let locks_dir = data_dir.join("locks");
    for path in sorted_real_dir_entries(&data_dir.join("merge-scratch")) {
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_dir() || meta.file_type().is_symlink() {
            if !opts.dry_run
                && orphan_scratch_lock_removable(&path, &meta, max_age, now, is_exclusively_locked)
            {
                let _ = fs::remove_file(&path);
            }
            continue;
        }
        let mut lock_os = path.as_os_str().to_os_string();
        lock_os.push(".lock");
        let lock_path = PathBuf::from(lock_os);
        let merge_locked = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.split_once("-pr").map(|(id, _)| id))
            .filter(|id| {
                !id.is_empty()
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            })
            .is_some_and(|id| is_exclusively_locked(&locks_dir.join(format!("{id}.merge.lock"))));
        if is_exclusively_locked(&lock_path) || merge_locked {
            continue;
        }
        // A live owner pid (`<id>-pr<N>-<pid>-<seq>`) means in use even when
        // its companion lock is not (or no longer) held.
        if scratch_owner_pid(&path)
            .is_some_and(|pid| crate::executors::reaper::is_alive(pid).unwrap_or(true))
        {
            continue;
        }
        let has_lock = fs::symlink_metadata(&lock_path).is_ok_and(|m| m.is_file());
        if has_lock || is_older_than(&meta, max_age, now) {
            remove_and_record(&path, true, opts.dry_run, report);
            if !opts.dry_run && has_lock && !path.exists() {
                let _ = fs::remove_file(&lock_path);
            }
        }
    }
}

/// Owner pid encoded in a merge-scratch dir name `<id>-pr<N>-<pid>-<seq>`.
fn scratch_owner_pid(path: &Path) -> Option<u32> {
    let name = path.file_name()?.to_str()?;
    let mut parts = name.rsplitn(3, '-');
    let seq = parts.next()?;
    let pid = parts.next()?;
    if !parts.next()?.contains("-pr") || !seq.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    pid.parse::<u32>().ok().filter(|p| *p > 1)
}

#[derive(Deserialize)]
struct SopStatusPeek {
    status: String,
}

/// A merge-scratch `.lock` whose scratch dir is gone is removable once it is
/// old and not held. `is_locked` takes a shared flock to probe the lock, so
/// age is checked first: a fresh lock is never probed, because its owner may
/// be about to `try_lock` it and would then time out.
pub(super) fn orphan_scratch_lock_removable(
    path: &Path,
    meta: &fs::Metadata,
    max_age: Duration,
    now: SystemTime,
    is_locked: impl FnOnce(&Path) -> bool,
) -> bool {
    meta.is_file()
        && path.extension().and_then(|e| e.to_str()) == Some("lock")
        && !path.with_extension("").exists()
        && is_older_than(meta, max_age, now)
        && !is_locked(path)
}

pub(super) fn is_older_than(meta: &fs::Metadata, max_age: Duration, now: SystemTime) -> bool {
    meta.modified()
        .ok()
        .is_some_and(|m| now.duration_since(m).unwrap_or(Duration::ZERO) >= max_age)
}

fn is_terminal_sop_file(path: &Path) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    let mut buf = Vec::new();
    if file
        .take(MAX_SOP_CHECKPOINT_BYTES + 1)
        .read_to_end(&mut buf)
        .is_err()
        || u64::try_from(buf.len()).unwrap_or(u64::MAX) > MAX_SOP_CHECKPOINT_BYTES
    {
        return false;
    }
    serde_json::from_slice::<SopStatusPeek>(&buf).is_ok_and(|peek| {
        matches!(
            peek.status.trim().to_ascii_lowercase().as_str(),
            "completed" | "aborted" | "failed"
        )
    })
}

pub(super) fn sorted_real_dir_entries(dir: &Path) -> Vec<PathBuf> {
    if !fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink()) {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    paths.sort();
    paths
}
