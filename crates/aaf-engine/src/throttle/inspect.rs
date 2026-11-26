//! Read-only non-blocking inspection of `<data_dir>/throttle` lock files.

use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThrottleSnapshot {
    pub throttle_dir_exists: bool,
    /// True only when `drain.lock` is actively locked by a live holder.
    pub drain_lock_present: bool,
    /// Count of `heavy.*.lock` files actively locked by a live holder.
    pub throttle_slots: usize,
    /// Total `heavy.*.lock` files present on disk (active or idle).
    pub slot_lock_files: usize,
    pub guest_record_files: u32,
    /// Active waiters queued in `<data_dir>/throttle/queue/*.wait`.
    #[serde(default)]
    pub waiters: usize,
    /// Age in seconds of the oldest active waiter in `queue/*.wait`.
    #[serde(default)]
    pub oldest_wait_secs: Option<u64>,
}

/// Probes whether `path` is a regular file currently held under an exclusive
/// advisory lock. Uses a non-blocking shared lock and unlocks immediately.
pub(crate) fn is_exclusively_locked(path: &Path) -> bool {
    let is_reg =
        fs::symlink_metadata(path).is_ok_and(|m| !m.file_type().is_symlink() && m.is_file());
    if !is_reg {
        return false;
    }
    File::open(path).is_ok_and(|file| match file.try_lock_shared() {
        Ok(()) => {
            let _ = file.unlock();
            false
        }
        Err(TryLockError::WouldBlock) => true,
        Err(TryLockError::Error(_)) => false,
    })
}

/// Raw result of probing a single `heavy.<n>.lock` entry in `<data_dir>/throttle`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeavySlotEntry {
    pub slot: usize,
    pub path: PathBuf,
    pub locked: bool,
}

/// Scans `<data_dir>/throttle` once (single `read_dir` + `flock` pass) and
/// returns both the aggregate [`ThrottleSnapshot`] and per-slot probe results.
pub fn scan_throttle(data_dir: &Path) -> (ThrottleSnapshot, Vec<HeavySlotEntry>) {
    let dir = data_dir.join("throttle");
    if !dir.is_dir() {
        return (ThrottleSnapshot::default(), Vec::new());
    }
    let drain_lock_present = is_exclusively_locked(&dir.join("drain.lock"));
    let (mut slot_lock_files, mut throttle_slots, mut slot_entries) = (0usize, 0usize, Vec::new());
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Some(slot_str) = name
                .strip_prefix("heavy.")
                .and_then(|s| s.strip_suffix(".lock"))
            {
                slot_lock_files = slot_lock_files.saturating_add(1);
                let path = entry.path();
                let locked = is_exclusively_locked(&path);
                throttle_slots = throttle_slots.saturating_add(usize::from(locked));
                if let Ok(slot) = slot_str.parse::<usize>() {
                    slot_entries.push(HeavySlotEntry { slot, path, locked });
                }
            }
        }
    }
    let guest_record_files = fs::read_dir(dir.join("guest")).map_or(0, |entries| {
        let count = entries
            .flatten()
            .filter(|e| e.file_name().to_str().is_some_and(|n| n.ends_with(".rec")))
            .count();
        u32::try_from(count).unwrap_or(u32::MAX)
    });
    let (waiters, oldest_wait_secs) = scan_wait_queue(&dir.join("queue"));
    let snap = ThrottleSnapshot {
        throttle_dir_exists: true,
        drain_lock_present,
        throttle_slots,
        slot_lock_files,
        guest_record_files,
        waiters,
        oldest_wait_secs,
    };
    (snap, slot_entries)
}

/// Read-only probe of active waiters in `<data_dir>/throttle/queue/*.wait`.
/// Vanishing files (`NotFound`) are ignored as normal races.
fn scan_wait_queue(queue_dir: &Path) -> (usize, Option<u64>) {
    let Ok(entries) = fs::read_dir(queue_dir) else {
        return (0, None);
    };
    let now = SystemTime::now();
    let (mut waiters, mut oldest) = (0usize, None::<u64>);
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.ends_with(".wait"))
        {
            continue;
        }
        let path = entry.path();
        if !is_exclusively_locked(&path) {
            continue;
        }
        waiters = waiters.saturating_add(1);
        if let Ok(age) = fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .ok_or(())
        {
            let secs = age.as_secs();
            oldest = Some(oldest.map_or(secs, |cur| cur.max(secs)));
        }
    }
    (waiters, oldest)
}

/// Inspects `<data_dir>/throttle` read-only using non-blocking shared lock probes.
pub fn inspect_throttle(data_dir: &Path) -> ThrottleSnapshot {
    scan_throttle(data_dir).0
}
