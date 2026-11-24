//! What a killed fingerprint run leaves in the git directory. `tree_fp`
//! removes its temporary index and object directory on every way out, but a
//! SIGKILL has no way out: the directory then keeps a blob of every changed
//! and untracked file for good.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::proc::{self, Failure};

pub(crate) const INDEX_PREFIX: &str = "aaf-index-";
pub(crate) const OBJECTS_PREFIX: &str = "aaf-objects-";

/// The process that made `name` (`aaf-index-<pid>-<seq>[.lock]`,
/// `aaf-objects-<pid>-<seq>`); `None` for any other name.
fn owner(name: &str) -> Option<u32> {
    let rest = name
        .strip_prefix(INDEX_PREFIX)
        .or_else(|| name.strip_prefix(OBJECTS_PREFIX))?;
    rest.split('-').next()?.parse().ok()
}

/// Removes the temporary files of fingerprint runs whose process is gone.
///
/// A run in progress is never touched: its process is alive, in this
/// process (`own_pid`, other threads included) or in another one. A pid
/// that was given to a new process keeps its leftovers until that one ends.
pub(crate) fn sweep(git_dir: &Path, own_pid: u32, alive: &dyn Fn(u32) -> bool) {
    // Housekeeping only: a directory that cannot be listed or an entry that
    // cannot be removed changes nothing about the fingerprint.
    let Ok(entries) = fs::read_dir(git_dir) else {
        return;
    };
    let mut alive_cache: HashMap<u32, bool> = HashMap::new();
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(owner) else {
            continue;
        };
        if pid == own_pid || *alive_cache.entry(pid).or_insert_with(|| alive(pid)) {
            continue;
        }
        let path = entry.path();
        // `symlink_metadata`: a link planted under such a name is removed
        // as a link, never followed.
        let removed = match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => fs::remove_dir_all(&path),
            Ok(_) => fs::remove_file(&path),
            Err(err) => Err(err),
        };
        // Another sweep got there first, or the entry is not ours to
        // remove: the next run tries again.
        let _ = removed;
    }
}

/// True unless `ps` says no process has this pid. When `ps` cannot answer
/// the process counts as alive: nothing is deleted on a guess.
pub(crate) fn process_alive(pid: u32) -> bool {
    let mut cmd = Command::new("/bin/ps");
    cmd.args(["-p", &pid.to_string(), "-o", "pid="]);
    // `ps -p` exits 1 when no listed process exists.
    !matches!(
        proc::run(cmd, "ps", None, Duration::from_secs(5)),
        Err(Failure::Exit { code: Some(1), .. })
    )
}
