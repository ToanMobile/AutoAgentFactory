//! Read-only non-blocking inspection of `<data_dir>/throttle` lock files.

use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};

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
    let snap = ThrottleSnapshot {
        throttle_dir_exists: true,
        drain_lock_present,
        throttle_slots,
        slot_lock_files,
        guest_record_files,
    };
    (snap, slot_entries)
}

/// Inspects `<data_dir>/throttle` read-only using non-blocking shared lock probes.
pub fn inspect_throttle(data_dir: &Path) -> ThrottleSnapshot {
    scan_throttle(data_dir).0
}
