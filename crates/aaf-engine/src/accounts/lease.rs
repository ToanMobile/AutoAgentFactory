//! Exclusive per-account lease backed by an advisory file lock.

use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};

use super::AccountsError;
use aaf_fp::fs_private as private;

/// Holds `leases/<account_id>.lock` locked for as long as the value lives.
/// The OS drops the lock when the file handle closes, so a crashed process
/// can never leave an account stuck.
#[derive(Debug)]
pub struct Lease {
    path: PathBuf,
    file: File,
}

impl Lease {
    /// `Ok(None)` when another handle (this process or any other) holds the
    /// lease.
    pub fn try_acquire(
        leases_dir: &Path,
        account_id: &str,
    ) -> Result<Option<Lease>, AccountsError> {
        if !is_safe_file_stem(account_id) {
            return Err(AccountsError::InvalidAccountId(account_id.to_string()));
        }
        let path = leases_dir.join(format!("{account_id}.lock"));
        let io = |source| AccountsError::Io {
            path: path.display().to_string(),
            source,
        };
        private::create_dir(leases_dir).map_err(io)?;
        // A planted link would make us lock (and create) a file elsewhere.
        if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(AccountsError::SymlinkLease {
                path: path.display().to_string(),
            });
        }
        let file = private::open_file(&path).map_err(io)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Lease { path, file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(source)) => Err(io(source)),
        }
    }

    /// Blocking exclusive lock under `leases_dir` used to serialize multi-slot
    /// check-and-acquire operations (e.g. `next_where_keeping_one`) across
    /// concurrent `aaf run` processes so two repos cannot both observe the same
    /// spare slot and drain the pool to zero.
    pub fn acquire_blocking(leases_dir: &Path, lock_name: &str) -> Result<Lease, AccountsError> {
        if !is_safe_file_stem(lock_name) {
            return Err(AccountsError::InvalidAccountId(lock_name.to_string()));
        }
        let path = leases_dir.join(format!("{lock_name}.lock"));
        let io = |source| AccountsError::Io {
            path: path.display().to_string(),
            source,
        };
        private::create_dir(leases_dir).map_err(io)?;
        if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(AccountsError::SymlinkLease {
                path: path.display().to_string(),
            });
        }
        let file = private::open_file(&path).map_err(io)?;
        file.lock().map_err(io)?;
        Ok(Lease { path, file })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Pure read-only probe: returns `true` when every concurrency slot for
    /// `account_id` is currently held under an exclusive advisory lock. Never
    /// creates directories, lock files, or exclusive locks.
    pub fn all_slots_leased_ro(leases_dir: &Path, account_id: &str, max_concurrent: u32) -> bool {
        if !is_safe_file_stem(account_id) {
            return false;
        }
        let slots = max_concurrent.max(1);
        for slot in 0..slots {
            let name = match slot {
                0 => account_id.to_string(),
                n => format!("{account_id}.slot{n}"),
            };
            let path = leases_dir.join(format!("{name}.lock"));
            let is_reg = fs::symlink_metadata(&path)
                .is_ok_and(|m| !m.file_type().is_symlink() && m.is_file());
            if !is_reg {
                return false;
            }
            let locked = File::open(&path).is_ok_and(|file| match file.try_lock_shared() {
                Ok(()) => {
                    let _ = file.unlock();
                    false
                }
                Err(TryLockError::WouldBlock) => true,
                Err(TryLockError::Error(_)) => false,
            });
            if !locked {
                return false;
            }
        }
        true
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        // Closing the handle is not enough on its own: a child process that
        // was forked a moment ago and has not reached exec yet still holds a
        // duplicate of it, which would keep the lock for that instant.
        // Ignoring a failure is correct: the close that follows releases
        // the lock anyway once no duplicate is left.
        let _ = self.file.unlock();
    }
}

fn is_safe_file_stem(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('.')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}
