//! Slots as an `aaf` from before the throttle had its own directory holds
//! them: lock files in `<data_dir>/locks`, next to the factory's run,
//! commit and merge locks. Such a process may still be running a gate when
//! a newer `aaf` starts, so the newer one looks there too and counts a slot
//! taken while its old lock file is locked or names a command that still
//! runs.
//!
//! This works in one direction only: the old `aaf` never looks into
//! `throttle/`, so it does not see a holder of this version. And a guest
//! ([`super::paths`]) cannot look into `locks/` at all, which its sandbox
//! closes with the rest of the data dir: for it these slots read as free.

use std::fs::{self, File, TryLockError};
use std::path::Path;

use super::orphans::orphan_in;
use super::paths::{legacy_drain_path, legacy_slot_path};
use super::probe::GroupState;
use super::slot_file::{is_locked, unlock_all};
use super::slots::names_token;
use super::token::SlotToken;

pub(super) enum Holder {
    /// A live process holds the old lock.
    Live,
    /// Nobody holds it, but the command of a killed holder still runs.
    Orphan(u32),
}

pub(super) fn is_held(data_dir: &Path, index: usize) -> bool {
    is_locked(&legacy_slot_path(data_dir, index))
}

fn text(data_dir: &Path, index: usize) -> String {
    // Missing or unreadable content names no holder.
    fs::read_to_string(legacy_slot_path(data_dir, index)).unwrap_or_default()
}

/// The record of a live holder of the old lock; `None` when nobody holds it.
pub(super) fn held_text(data_dir: &Path, index: usize) -> Option<String> {
    is_held(data_dir, index).then(|| text(data_dir, index))
}

/// Who occupies slot `index` through its old lock file, if anybody.
pub(super) fn holder(
    data_dir: &Path,
    index: usize,
    group_state: &dyn Fn(u32) -> Option<GroupState>,
) -> Option<Holder> {
    // No such file (the usual case), or no right to open it: no old holder
    // that this process could know of.
    let file = File::options()
        .write(true)
        .open(legacy_slot_path(data_dir, index))
        .ok()?;
    match file.try_lock() {
        Ok(()) => {
            let left = text(data_dir, index);
            let orphan = orphan_in(&left, group_state);
            if orphan.is_none() && !left.is_empty() {
                // Left by a holder that was killed and whose command is
                // gone: emptied like the old version did, so that it is not
                // looked up in the process table on every attempt. If that
                // fails the next look does the same again.
                let _ = file.set_len(0);
            }
            unlock_all(std::slice::from_ref(&file));
            orphan.map(Holder::Orphan)
        }
        Err(TryLockError::WouldBlock) => Some(Holder::Live),
        // Cannot be probed: not known to be held.
        Err(TryLockError::Error(_)) => None,
    }
}

/// True when the caller runs below a command whose `aaf` took slot `index`
/// through the old lock file and wrote `token` into it.
pub(super) fn holds_token(
    data_dir: &Path,
    index: usize,
    token: &SlotToken,
    group_state: &dyn Fn(u32) -> Option<GroupState>,
) -> bool {
    let written = || names_token(&text(data_dir, index), token);
    if is_held(data_dir, index) {
        return written();
    }
    let left = text(data_dir, index);
    written() && orphan_in(&left, group_state).is_some() && written()
}

/// True while a live process holds the old drain lock.
pub(super) fn draining(data_dir: &Path) -> bool {
    is_locked(&legacy_drain_path(data_dir))
}
