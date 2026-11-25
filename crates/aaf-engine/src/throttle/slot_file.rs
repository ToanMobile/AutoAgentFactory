//! One slot's lock file as this process has it open, and where the
//! holder's record is kept.
//!
//! A holder outside the sandbox writes its reservation and process group
//! into the lock file itself, and its token into a file of the closed part
//! of the data dir ([`super::paths::token_path`]): an agent may read the
//! first and learns nothing from it that opens the slot. A guest
//! ([`super::paths`]) can lock the lock file and not write it, so a guest
//! that holds the slot keeps token and reservation in `guest/heavy.<n>.rec`.
//!
//! Which of the two is read is decided by the lock file alone, which no
//! guest can write: while it has content, the holder is one outside the
//! sandbox and a guest file is not looked at. A guest can therefore put
//! whatever it likes into `guest/` without changing what is known about a
//! holder outside the sandbox. That holds from the holder's first write
//! ([`SlotFile::claim`], right after the lock is taken): in the instant
//! between taking the lock and that write, nothing tells the holder from a
//! guest, because taking a lock leaves no trace another process can read.

use std::fs::{self, File, TryLockError};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

use super::paths::{
    ensure_closed_dir, guest_record_path, open_guest_file, read_guest_file, slot_path, token_path,
};
use aaf_fp::fs_private as private;

/// Where this process writes what it holds a slot with.
#[derive(Debug)]
enum Record {
    /// Outside the sandbox: the closed token file; the rest goes into the
    /// lock file.
    Owner { token: File },
    /// A guest: one file for both; the lock file is open for reading only.
    Guest { record: File },
}

#[derive(Debug)]
pub(super) struct SlotFile {
    lock: File,
    record: Record,
}

fn rewrite(mut file: &File, text: &str) -> io::Result<()> {
    // One write over the same bytes, so a reader sees the old or the new
    // text and never half of one.
    file.seek(SeekFrom::Start(0))?;
    file.write_all(text.as_bytes())?;
    file.set_len(text.len() as u64)
}

impl SlotFile {
    /// Opens (and, outside a sandbox, creates) the lock file of slot `index`.
    /// `None` for a guest when the file does not exist: a guest cannot
    /// create one, so for it there is no such slot.
    pub(super) fn open(data_dir: &Path, index: usize) -> io::Result<Option<SlotFile>> {
        let path = slot_path(data_dir, index);
        match private::open_file(&path) {
            Ok(lock) => {
                ensure_closed_dir(data_dir)?;
                let token = private::open_file(&token_path(data_dir, index))?;
                Ok(Some(SlotFile {
                    lock,
                    record: Record::Owner { token },
                }))
            }
            // The sandbox of an agent run: reading and locking is all the
            // file allows.
            Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
                let lock = match File::open(&path) {
                    Ok(lock) => lock,
                    Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
                    Err(err) => return Err(err),
                };
                let record = open_guest_file(&guest_record_path(data_dir, index))?;
                Ok(Some(SlotFile {
                    lock,
                    record: Record::Guest { record },
                }))
            }
            Err(err) => Err(err),
        }
    }

    pub(super) fn is_guest(&self) -> bool {
        matches!(self.record, Record::Guest { .. })
    }

    pub(super) fn try_lock(&self) -> Result<(), TryLockError> {
        self.lock.try_lock()
    }

    /// Explicit, instead of relying on the close: a child process that is
    /// being spawned at this moment (by any thread) briefly holds a copy of
    /// every descriptor, and the lock would stay taken until it has exec'ed.
    pub(super) fn unlock(&self) {
        unlock_all(std::slice::from_ref(&self.lock));
    }

    /// True while the lock file itself has content: the record of a holder
    /// outside the sandbox, present or killed.
    pub(super) fn lock_has_content(&self) -> io::Result<bool> {
        Ok(self.lock.metadata()?.len() > 0)
    }

    /// Writes this holder's record: `token_line` where only its own kind
    /// reads it, `rest` (reservation, process group) beside it.
    pub(super) fn write_record(&self, token_line: &str, rest: &str) -> io::Result<()> {
        match &self.record {
            Record::Owner { token } => {
                rewrite(token, token_line)?;
                rewrite(&self.lock, rest)
            }
            Record::Guest { record } => rewrite(record, &format!("{token_line}{rest}")),
        }
    }

    /// Puts `placeholder` where this holder's record goes, as the first
    /// write after the lock is taken. Outside the sandbox the lock file then
    /// has content, so from here on no reader takes a record in `guest/`
    /// for this holder's; the token file is emptied after that, so a
    /// reader never pairs the placeholder with an earlier holder's token.
    pub(super) fn claim(&self, placeholder: &str) -> io::Result<()> {
        match &self.record {
            Record::Owner { token } => {
                rewrite(&self.lock, placeholder)?;
                token.set_len(0)
            }
            Record::Guest { record } => rewrite(record, placeholder),
        }
    }

    /// Removes the record of a holder, this one's or a killed one's, while
    /// the slot is locked. A guest cannot remove what a holder outside the
    /// sandbox left in the lock file; for it that is not an error.
    pub(super) fn clear_record(&self) -> io::Result<()> {
        match &self.record {
            Record::Owner { token } => {
                token.set_len(0)?;
                self.lock.set_len(0)
            }
            Record::Guest { record } => record.set_len(0),
        }
    }

    /// For a holder outside the sandbox that takes the slot: what the last
    /// guest left in its record file must not be read as this holder's in
    /// the moment before its own record is written.
    pub(super) fn drop_guest_leftover(&self, data_dir: &Path, index: usize) {
        if self.is_guest() {
            return;
        }
        let path = guest_record_path(data_dir, index);
        if fs::symlink_metadata(&path).is_ok_and(|meta| meta.len() > 0) {
            // Whatever a guest planted under this name goes with it. If it
            // cannot be removed it is still never read while the lock file
            // has content, which it has from the next statement on.
            let _ = fs::remove_file(&path);
        }
    }
}

/// The lock file's own text: reservation and process group of a holder
/// outside the sandbox.
pub(super) fn lock_text(data_dir: &Path, index: usize) -> String {
    // Unreadable content names no holder.
    fs::read_to_string(slot_path(data_dir, index)).unwrap_or_default()
}

/// The closed token file's text: empty for whoever may not read it.
pub(super) fn owner_token_text(data_dir: &Path, index: usize) -> String {
    fs::read_to_string(token_path(data_dir, index)).unwrap_or_default()
}

/// Reservation and process group of whoever holds slot `index`: the lock
/// file's text, else the guest record. Says nothing about the slot being
/// held.
pub(super) fn record_text(data_dir: &Path, index: usize) -> String {
    let own = lock_text(data_dir, index);
    if own.is_empty() {
        read_guest_file(&guest_record_path(data_dir, index))
    } else {
        own
    }
}

/// The text that names the token of whoever holds slot `index`: the closed
/// token file for a holder outside the sandbox, else the guest record.
pub(super) fn holder_token_text(data_dir: &Path, index: usize) -> String {
    if lock_text(data_dir, index).is_empty() {
        read_guest_file(&guest_record_path(data_dir, index))
    } else {
        owner_token_text(data_dir, index)
    }
}

/// Unlocks explicitly instead of relying on the close: a child process that
/// is being spawned at this moment (by any thread) briefly holds a copy of
/// every descriptor, and the lock would stay taken until it has exec'ed.
pub(super) fn unlock_all(files: &[File]) {
    for file in files {
        // Nothing useful can be done if this fails, and the close that
        // follows releases the lock anyway.
        let _ = file.unlock();
    }
}

/// True while some process holds the lock on `path`; false when the file
/// is missing or cannot be probed.
pub(super) fn is_locked(path: &Path) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    // Shared, so that two processes looking at the same time do not take
    // each other for the holder.
    match file.try_lock_shared() {
        Ok(()) => {
            unlock_all(std::slice::from_ref(&file));
            false
        }
        Err(TryLockError::WouldBlock) => true,
        Err(TryLockError::Error(_)) => false,
    }
}
