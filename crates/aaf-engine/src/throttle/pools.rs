//! Layer-1 correctness locks for concurrent heavy builds.
//!
//! Even when `machine.exclusive_build_tools = false` allows measured builds
//! (e.g. Unity 2.5 GB + Gradle 4.5 GB) to share Layer-2 capacity slots by RAM,
//! two builds of the SAME checkout must never stomp each other's build dirs,
//! and concurrent Unity Editors are capped by `machine.max_unity_parallel`.
//!
//! Lock acquisition order is strictly:
//!   1. `UnityPoolLock` (`tool.unity.<i >.lock`, Unity only)
//!   2. `WorktreeLock` (`worktree.<sha256(canonical_cwd)>.lock`, any heavy tool)
//!   3. Layer-2 capacity slots (`heavy.<i >.lock`)
//!
//! Fixed total order prevents deadlocks across all processes.

use std::fs::{self, File, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

use aaf_domain::ports::Clock;

use super::heavy::HeavyTool;
use super::paths::{ensure_closed_dir, guest_dir, is_guest, open_guest_file};
use super::{Backoff, Sleeper};
use crate::db::private;

/// Held Layer-1 correctness locks; released automatically on drop.
#[derive(Debug, Default)]
pub struct CorrectnessGuards {
    _unity: Option<File>,
    _worktree: Option<File>,
}

fn lock_dir(data_dir: &Path) -> io::Result<PathBuf> {
    let base = if is_guest(data_dir) {
        guest_dir(data_dir)
    } else {
        ensure_closed_dir(&data_dir.join("throttle"))?
    };
    let dir = base.join("pools");
    if is_guest(data_dir) {
        fs::create_dir_all(&dir)?;
    } else {
        ensure_closed_dir(&dir)?;
    }
    Ok(dir)
}

fn open_pool_file(data_dir: &Path, path: &Path) -> io::Result<File> {
    if is_guest(data_dir) {
        open_guest_file(path)
    } else {
        private::open_file(path)
    }
}

fn try_lock_one(data_dir: &Path, path: &Path) -> io::Result<Option<File>> {
    let file = open_pool_file(data_dir, path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(err)) => Err(err),
    }
}

fn try_lock_unity(data_dir: &Path, max_unity: u32) -> io::Result<Option<File>> {
    let cap = max_unity.max(1);
    let dir = lock_dir(data_dir)?;
    for idx in 0..cap {
        let path = dir.join(format!("tool.unity.{idx}.lock"));
        if let Some(file) = try_lock_one(data_dir, &path)? {
            return Ok(Some(file));
        }
    }
    Ok(None)
}

fn worktree_lock_path(data_dir: &Path, cwd: &Path) -> io::Result<PathBuf> {
    let canonical = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let id = aaf_fp::sha256_hex(canonical.to_string_lossy().as_bytes());
    let short = &id[..16];
    Ok(lock_dir(data_dir)?.join(format!("worktree.{short}.lock")))
}

/// Parameters for acquiring Layer-1 correctness locks.
pub struct CorrectnessAsk<'a> {
    pub tool: Option<HeavyTool>,
    pub cwd: &'a Path,
    pub max_unity_parallel: u32,
    pub wait_limit_secs: Option<u64>,
    pub jitter_seed: u64,
}

/// Acquires Layer-1 correctness locks for `ask.tool` in `ask.cwd`, respecting
/// `ask.wait_limit_secs` (returns `Ok(None)` on timeout).
pub fn acquire_correctness(
    data_dir: &Path,
    ask: &CorrectnessAsk<'_>,
    clock: &dyn Clock,
    sleeper: &dyn Sleeper,
) -> io::Result<Option<CorrectnessGuards>> {
    let Some(tool) = ask.tool else {
        return Ok(Some(CorrectnessGuards::default()));
    };
    let started = clock.now();
    let expired = |now: i64| {
        let waited = u64::try_from(now - started).unwrap_or(0);
        ask.wait_limit_secs.is_some_and(|limit| waited >= limit)
    };
    let mut backoff = Backoff::new(ask.jitter_seed ^ 0x9E37_79B9_7F4A_7C15);

    // 1. Unity pool lock (Unity only).
    let unity = if tool == HeavyTool::Unity {
        loop {
            if let Some(file) = try_lock_unity(data_dir, ask.max_unity_parallel)? {
                break Some(file);
            }
            if expired(clock.now()) {
                return Ok(None);
            }
            sleeper.sleep(backoff.next_delay());
        }
    } else {
        None
    };

    // 2. Per-worktree lock (any heavy build tool).
    let wt_path = worktree_lock_path(data_dir, ask.cwd)?;
    let worktree = loop {
        if let Some(file) = try_lock_one(data_dir, &wt_path)? {
            break Some(file);
        }
        if expired(clock.now()) {
            return Ok(None);
        }
        sleeper.sleep(backoff.next_delay());
    };

    Ok(Some(CorrectnessGuards {
        _unity: unity,
        _worktree: worktree,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::throttle::tests::support::Rig;
    use tempfile::TempDir;

    #[test]
    fn unity_pool_caps_concurrent_unity_without_blocking_gradle() {
        let data = TempDir::new().unwrap();
        let wt1 = TempDir::new().unwrap();
        let wt2 = TempDir::new().unwrap();
        let rig = Rig::calm();

        let ask = |tool, cwd, seed| CorrectnessAsk {
            tool: Some(tool),
            cwd,
            max_unity_parallel: 1,
            wait_limit_secs: Some(0),
            jitter_seed: seed,
        };

        let g1 = acquire_correctness(
            data.path(),
            &ask(HeavyTool::Unity, wt1.path(), 1),
            &*rig.clock,
            &rig.sleeper,
        )
        .unwrap();
        assert!(g1.is_some());

        // Second Unity on a different repo is blocked by max_unity_parallel=1.
        let g2 = acquire_correctness(
            data.path(),
            &ask(HeavyTool::Unity, wt2.path(), 2),
            &*rig.clock,
            &rig.sleeper,
        )
        .unwrap();
        assert!(g2.is_none());

        // Gradle on another repo is NOT blocked by the Unity pool!
        let g3 = acquire_correctness(
            data.path(),
            &ask(HeavyTool::Gradle, wt2.path(), 3),
            &*rig.clock,
            &rig.sleeper,
        )
        .unwrap();
        assert!(g3.is_some());

        // A second Gradle in the SAME checkout IS blocked by WorktreeLock!
        let g4 = acquire_correctness(
            data.path(),
            &ask(HeavyTool::Gradle, wt2.path(), 4),
            &*rig.clock,
            &rig.sleeper,
        )
        .unwrap();
        assert!(g4.is_none());
    }
}
