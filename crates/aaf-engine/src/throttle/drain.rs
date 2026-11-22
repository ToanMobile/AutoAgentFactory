//! The drain lock: how a command that must run alone gets its turn on a
//! machine whose slots never all empty at once.

use std::fs::{File, TryLockError};
use std::io;
use std::path::Path;

use super::legacy;
use super::paths::{drain_path, is_guest};
use super::slots::{is_locked, locks_dir, unlock_all};
use crate::db::private;

/// Held by a command that runs alone and has waited long enough: while it
/// is held no measured command is admitted, so the slots empty and the
/// waiter gets its turn. Dropping it (or the death of the process) opens
/// the door again.
#[derive(Debug)]
pub struct DrainGuard {
    file: File,
}

impl Drop for DrainGuard {
    fn drop(&mut self) {
        unlock_all(std::slice::from_ref(&self.file));
    }
}

/// Starts draining; `None` while another command already does.
pub fn try_drain(data_dir: &Path) -> io::Result<Option<DrainGuard>> {
    locks_dir(data_dir)?;
    let path = drain_path(data_dir);
    let file = if is_guest(data_dir) {
        // A guest may lock the file and not write it; the lock is all a
        // drain is.
        File::open(&path)?
    } else {
        private::open_file(&path)?
    };
    match file.try_lock() {
        Ok(()) => Ok(Some(DrainGuard { file })),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(err)) => Err(err),
    }
}

/// True while a live process holds the drain lock.
pub(super) fn draining(data_dir: &Path) -> bool {
    // A file that cannot be opened or probed counts as "not draining": the
    // drain only keeps the queue fair, and an I/O error must not close the
    // machine to every command.
    is_locked(&drain_path(data_dir)) || legacy::draining(data_dir)
}
