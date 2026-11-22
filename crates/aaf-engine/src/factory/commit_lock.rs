//! Per-repository commit lock: one `git commit` of the factory at a time,
//! across every `aaf` process of this user. A commit fires the repository's
//! pre-commit hook, and N of them at once (`--max-parallel N`, each hook
//! starting a build) froze a machine.

use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use aaf_domain::ports::{PortError, PortResult};

use crate::db::private;

/// The holder is inside one `git commit`; nothing is gained by asking more
/// often than this.
const POLL: Duration = Duration::from_millis(50);

/// `<locks_dir>/<repo_id>.commit.lock`.
pub(super) fn lock_path(locks_dir: &Path, repo_root: &Path) -> PathBuf {
    locks_dir.join(format!("{}.commit.lock", aaf_fp::repo_id(repo_root)))
}

/// Holds the exclusive lock until dropped. The kernel also drops it when the
/// process dies, so a crashed run never leaves a repository locked.
pub(super) struct CommitLock {
    file: File,
}

impl CommitLock {
    pub(super) fn acquire(path: &Path, timeout: Duration) -> PortResult<CommitLock> {
        let io = |error: std::io::Error| PortError::Io(format!("{}: {error}", path.display()));
        if let Some(parent) = path.parent() {
            private::create_dir(parent).map_err(io)?;
        }
        let file = private::open_file(path).map_err(io)?;
        let deadline = Instant::now() + timeout;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(CommitLock { file }),
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => thread::sleep(POLL),
                Err(TryLockError::WouldBlock) => {
                    return Err(PortError::Io(format!(
                        "{}: another aaf process is still committing in this repository",
                        path.display()
                    )));
                }
                Err(TryLockError::Error(error)) => return Err(io(error)),
            }
        }
    }
}

impl Drop for CommitLock {
    fn drop(&mut self) {
        // Closing the file releases the lock anyway; unlocking makes it
        // immediate even while a child being spawned holds a copy of the
        // descriptor.
        let _ = self.file.unlock();
    }
}
