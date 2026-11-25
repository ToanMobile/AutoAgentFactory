//! Bounded reaping of stale orphaned heavy-slot lock files so a crashed process
//! group cannot hang admission forever.

use std::fs::{self, File, TryLockError};
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime};

use super::super::paths::{legacy_slot_path, slot_path};
use super::super::slot_file::{SlotFile, unlock_all};

fn age_at_least(path: &Path, min_age: Option<Duration>) -> bool {
    let Some(min_age) = min_age else {
        return true;
    };
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .is_ok_and(|mtime| {
            SystemTime::now()
                .duration_since(mtime)
                .unwrap_or(Duration::ZERO)
                >= min_age
        })
}

fn reap_impl(data_dir: &Path, ceiling: usize, min_age: Option<Duration>) -> io::Result<usize> {
    let ceiling = ceiling.max(1);
    let mut reaped = 0usize;

    for index in 0..ceiling {
        let path = slot_path(data_dir, index);
        if path.exists()
            && age_at_least(&path, min_age)
            && let Some(file) = SlotFile::open(data_dir, index)?
            && !file.is_guest()
        {
            match file.try_lock() {
                Ok(()) => {
                    if file.lock_has_content().unwrap_or(false) && file.clear_record().is_ok() {
                        super::orphan_since::clear(data_dir, index);
                        reaped += 1;
                    }
                    file.unlock();
                }
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(err)) => return Err(err),
            }
        }

        let legacy = legacy_slot_path(data_dir, index);
        if legacy.exists()
            && age_at_least(&legacy, min_age)
            && let Ok(file) = File::options().write(true).open(&legacy)
            && file.try_lock().is_ok()
        {
            let non_empty = file.metadata().is_ok_and(|meta| meta.len() > 0);
            if non_empty && file.set_len(0).is_ok() {
                super::orphan_since::clear(data_dir, index);
                reaped += 1;
            }
            unlock_all(std::slice::from_ref(&file));
        }
    }

    Ok(reaped)
}

/// Clears any unlocked slot lock files (and legacy slot lock files) in `0..ceiling`
/// that still carry leftover orphan records. Never touches a slot held by a live
/// advisory lock.
pub fn reap_orphaned_slots(data_dir: &Path, ceiling: usize) -> io::Result<usize> {
    reap_impl(data_dir, ceiling, None)
}

/// Like [`reap_orphaned_slots`], but only reaps unlocked orphaned slot lock files
/// whose on-disk modification age is at least `max_age`.
pub fn reap_stale_orphans(data_dir: &Path, ceiling: usize, max_age: Duration) -> io::Result<usize> {
    reap_impl(data_dir, ceiling, Some(max_age))
}
