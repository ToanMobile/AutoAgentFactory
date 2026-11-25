//! Crash-safe FIFO waiter queue under `<data_dir>/throttle/queue/`.
//!
//! Without FIFO ordering, a waiter whose polling backoff has grown to 8 s is
//! repeatedly jumped by newly arrived waiters polling at 250 ms (LIFO
//! starvation). Each waiting host process takes a timestamped ticket file
//! locked with `flock`; when it dies or gets a slot the lock drops and stale
//! files are cleaned up opportunistically.
//!
//! Waiters only hold a ticket while blocked on slot capacity (`SlotsTaken` /
//! `Draining`), never while blocked on `NotEnoughMemory`, so a large job
//! waiting for RAM never head-of-line blocks a smaller job that fits.
//! Sandboxed guests skip queue writes gracefully.

use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::paths::{is_guest, locks_dir, throttle_dir};
use super::slot_file::unlock_all;
use aaf_fp::fs_private as private;

const QUEUE_DIR: &str = "queue";
const EXT: &str = ".wait";
static SEQ: AtomicU64 = AtomicU64::new(0);

/// An active FIFO queue ticket held by a waiting process.
#[derive(Debug)]
pub struct WaitTicket {
    name: String,
    path: PathBuf,
    file: File,
}

impl WaitTicket {
    /// Registers a locked FIFO ticket for `arrived_at` (epoch seconds from the
    /// throttle clock). Returns `None` for sandboxed guests or on any I/O error
    /// so queue bookkeeping can never break admission.
    pub fn join(data_dir: &Path, arrived_at: i64) -> Option<WaitTicket> {
        if is_guest(data_dir) {
            return None;
        }
        let dir = queue_dir(data_dir).ok()?;
        let ts = u64::try_from(arrived_at).unwrap_or(0);
        let pid = std::process::id();
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let name = format!("{ts:016}-{pid:08}-{seq:08}{EXT}");
        let path = dir.join(&name);
        let draft = dir.join(format!("{name}.tmp"));
        let file = private::open_file(&draft).ok()?;
        if file.try_lock().is_err() {
            let _ = fs::remove_file(&draft);
            return None;
        }
        if fs::rename(&draft, &path).is_err() {
            unlock_all(std::slice::from_ref(&file));
            let _ = fs::remove_file(&draft);
            return None;
        }
        Some(WaitTicket { name, path, file })
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for WaitTicket {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        unlock_all(std::slice::from_ref(&self.file));
    }
}

fn queue_dir(data_dir: &Path) -> std::io::Result<PathBuf> {
    let base = locks_dir(data_dir)?;
    let dir = base.join(QUEUE_DIR);
    private::create_dir(&dir)?;
    Ok(dir)
}

/// Counts live waiting processes strictly ahead of `mine` (or all live waiters
/// when `mine` is `None`). Stale unlocked ticket files from crashed processes
/// are removed opportunistically by host processes.
pub fn older_waiters(data_dir: &Path, mine: Option<&WaitTicket>) -> usize {
    older_waiters_than(data_dir, mine, i64::MAX)
}

/// The second of a ticket name; `None` for a name this module did not make.
fn ticket_secs(name: &str) -> Option<u64> {
    name.get(..16)?.parse().ok()
}

/// [`older_waiters`] for a caller that arrived at `arrived_at` (epoch
/// seconds from the throttle clock). Without a ticket of its own (a sandboxed
/// guest cannot write one, a host has none before it first waits) only
/// tickets taken no later than its arrival are ahead of it: counting the
/// newer ones too let every host that queued after a guest hold it back.
pub fn older_waiters_than(data_dir: &Path, mine: Option<&WaitTicket>, arrived_at: i64) -> usize {
    let arrived = u64::try_from(arrived_at).unwrap_or(0);
    let guest = is_guest(data_dir);
    let dir = throttle_dir(data_dir).join(QUEUE_DIR);
    let Ok(entries) = fs::read_dir(&dir) else {
        return 0;
    };
    let my_name = mine.map(WaitTicket::name);
    let mut ahead = 0usize;
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !name.ends_with(EXT) {
            continue;
        }
        if let Some(mine) = my_name
            && name.as_str() >= mine
        {
            continue;
        }
        if my_name.is_none() && ticket_secs(&name).is_some_and(|secs| secs > arrived) {
            continue;
        }
        let path = entry.path();
        let Ok(file) = File::open(&path) else {
            continue;
        };
        match file.try_lock_shared() {
            Err(TryLockError::WouldBlock) => ahead += 1,
            Ok(()) => {
                // Release shared probe before attempting exclusive cleanup.
                unlock_all(std::slice::from_ref(&file));
                drop(file);
                if !guest
                    && let Ok(rw) = File::options().read(true).write(true).open(&path)
                    && rw.try_lock().is_ok()
                {
                    // Holder exited or crashed; remove the stale ticket while locked.
                    let _ = fs::remove_file(&path);
                    unlock_all(std::slice::from_ref(&rw));
                }
            }
            Err(TryLockError::Error(_)) => {}
        }
    }
    ahead
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn fifo_orders_older_waiters_ahead_of_newer_and_cleans_stale_tickets() {
        let data = TempDir::new().unwrap();
        let first = WaitTicket::join(data.path(), 1_000).unwrap();
        let second = WaitTicket::join(data.path(), 1_005).unwrap();

        // First sees nobody ahead; second sees first ahead; newcomer sees both.
        assert_eq!(older_waiters(data.path(), Some(&first)), 0);
        assert_eq!(older_waiters(data.path(), Some(&second)), 1);
        assert_eq!(older_waiters(data.path(), None), 2);
        // Without a ticket, only tickets taken by its arrival are ahead.
        assert_eq!(older_waiters_than(data.path(), None, 999), 0);
        assert_eq!(older_waiters_than(data.path(), None, 1_000), 1);
        assert_eq!(older_waiters_than(data.path(), None, 1_005), 2);
        assert_eq!(older_waiters_than(data.path(), Some(&second), 0), 1);

        // Stale unlocked file from an older dead process is cleaned up and ignored.
        let stale = data
            .path()
            .join("throttle/queue/0000000000000500-00000001-00000000.wait");
        fs::write(&stale, "").unwrap();
        assert_eq!(older_waiters(data.path(), Some(&first)), 0);
        assert!(!stale.exists());

        // Dropping the first ticket immediately promotes the second.
        drop(first);
        assert_eq!(older_waiters(data.path(), Some(&second)), 0);
    }
}
