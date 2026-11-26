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
use std::path::Path;

use aaf_domain::ports::Clock;

use super::heavy::HeavyTool;
use super::paths::open_pool_file;
use super::slot_file::unlock_all;
use super::{Backoff, Budget, Sleeper};

/// Held Layer-1 correctness locks; released automatically on drop.
#[derive(Debug, Default)]
pub struct CorrectnessGuards {
    unity: Option<File>,
    gradle: Option<File>,
    worktree: Option<File>,
    waited: bool,
}

impl Drop for CorrectnessGuards {
    fn drop(&mut self) {
        // Unlocked explicitly like every other lock here ([`unlock_all`]):
        // the close alone frees it only once each child being spawned at
        // this moment has exec'ed, and the next build of this checkout,
        // or one with `--timeout 0`, would find it still taken.
        unlock_all(self.worktree.as_slice());
        unlock_all(self.gradle.as_slice());
        unlock_all(self.unity.as_slice());
    }
}

impl CorrectnessGuards {
    /// True when a lock was taken only after sleeping for it.
    pub fn waited(&self) -> bool {
        self.waited
    }
}

fn try_lock_one(data_dir: &Path, filename: &str) -> io::Result<Option<File>> {
    let file = open_pool_file(data_dir, filename)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(err)) => Err(err),
    }
}

fn try_lock_pool(data_dir: &Path, prefix: &str, cap: u32) -> io::Result<Option<File>> {
    for idx in 0..cap {
        let filename = format!("tool.{prefix}.{idx}.lock");
        if let Some(file) = try_lock_one(data_dir, &filename)? {
            return Ok(Some(file));
        }
    }
    Ok(None)
}

fn worktree_lock_name(cwd: &Path) -> String {
    let canonical = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let id = aaf_fp::sha256_hex(canonical.to_string_lossy().as_bytes());
    let short = &id[..16];
    format!("worktree.{short}.lock")
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
    let mut budget = Budget::new(ask.wait_limit_secs, clock.now());
    acquire_correctness_within(data_dir, ask, 0, clock, sleeper, &mut budget)
}

/// [`acquire_correctness`] spending `budget` (its limit, not
/// `ask.wait_limit_secs`), which the Layer-2 wait goes on spending.
/// Lock acquisition order: `UnityPool -> GradlePool -> Worktree -> Layer-2`.
pub(super) fn acquire_correctness_within(
    data_dir: &Path,
    ask: &CorrectnessAsk<'_>,
    max_gradle_parallel: u32,
    clock: &dyn Clock,
    sleeper: &dyn Sleeper,
    budget: &mut Budget,
) -> io::Result<Option<CorrectnessGuards>> {
    let Some(tool) = ask.tool else {
        return Ok(Some(CorrectnessGuards::default()));
    };
    let mut backoff = Backoff::new(ask.jitter_seed ^ 0x9E37_79B9_7F4A_7C15);
    let mut waited = false;

    // 1. Unity pool lock (Unity only).
    let unity = if tool == HeavyTool::Unity {
        loop {
            if let Some(file) = try_lock_pool(data_dir, "unity", ask.max_unity_parallel.max(1))? {
                break Some(file);
            }
            if budget.parent_exited() || budget.expired(clock.now()) {
                return Ok(None);
            }
            waited = true;
            budget.sleep(sleeper, backoff.next_delay());
        }
    } else {
        None
    };

    // 2. Gradle pool lock (Gradle only, when capped).
    let gradle = if tool == HeavyTool::Gradle && max_gradle_parallel > 0 {
        loop {
            if let Some(file) = try_lock_pool(data_dir, "gradle", max_gradle_parallel)? {
                break Some(file);
            }
            if budget.parent_exited() || budget.expired(clock.now()) {
                return Ok(None);
            }
            waited = true;
            budget.sleep(sleeper, backoff.next_delay());
        }
    } else {
        None
    };

    // 3. Per-worktree lock (any heavy build tool).
    let wt_name = worktree_lock_name(ask.cwd);
    let worktree = loop {
        if let Some(file) = try_lock_one(data_dir, &wt_name)? {
            break Some(file);
        }
        if budget.parent_exited() || budget.expired(clock.now()) {
            return Ok(None);
        }
        waited = true;
        budget.sleep(sleeper, backoff.next_delay());
    };

    Ok(Some(CorrectnessGuards {
        unity,
        gradle,
        worktree,
        waited,
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

    /// Dropping the guards frees the locks at once even while a child holds
    /// a copy of their descriptors, as a child that any thread is spawning
    /// does until it execs (audit a13 round 4: under parallel load the
    /// checkout stayed locked after `run` returned).
    #[test]
    fn dropped_guards_free_the_locks_while_a_child_holds_a_copy() {
        let data = TempDir::new().unwrap();
        let wt = TempDir::new().unwrap();
        let rig = Rig::calm();
        let ask = |seed| CorrectnessAsk {
            tool: Some(HeavyTool::Unity),
            cwd: wt.path(),
            max_unity_parallel: 1,
            wait_limit_secs: Some(0),
            jitter_seed: seed,
        };
        let held = acquire_correctness(data.path(), &ask(1), &*rig.clock, &rig.sleeper)
            .unwrap()
            .unwrap();
        let copy = |file: &Option<File>| file.as_ref().unwrap().try_clone().unwrap();
        // Stands for the fork-to-exec window of a sibling's spawn: a child
        // holding both descriptors for longer than the test runs.
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("60")
            .stdin(copy(&held.unity))
            .stdout(copy(&held.worktree))
            .spawn()
            .unwrap();
        drop(held);
        let again = acquire_correctness(data.path(), &ask(2), &*rig.clock, &rig.sleeper).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(
            again.is_some(),
            "the Unity and checkout locks outlived their guards"
        );
    }

    /// A clock and sleeper that keep milliseconds; the clock reads whole
    /// seconds like the real one.
    struct MillisTime {
        millis: std::sync::atomic::AtomicU64,
        slept: std::sync::Mutex<Vec<std::time::Duration>>,
    }

    impl Clock for MillisTime {
        fn now(&self) -> i64 {
            let millis = self.millis.load(std::sync::atomic::Ordering::SeqCst);
            i64::try_from(millis / 1_000).unwrap()
        }
    }

    impl Sleeper for MillisTime {
        fn sleep(&self, duration: std::time::Duration) {
            self.slept.lock().unwrap().push(duration);
            let millis = u64::try_from(duration.as_millis()).unwrap();
            self.millis
                .fetch_add(millis, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// The Layer-1 wait keeps `--timeout` to the millisecond as admission
    /// does: neither ended by the second turning just after the start nor
    /// a whole backoff sleep past the limit (audit a13 L1-R2-1).
    #[test]
    fn correctness_wait_limit_is_kept_to_the_millisecond() {
        use std::time::Duration;
        for (limit, start_millis, max_millis) in [
            (1u64, 1_000_999u64, 1_250u64),
            (1, 1_000_000, 1_250),
            (10, 1_000_000, 10_250),
            (10, 1_000_999, 10_250),
        ] {
            let data = TempDir::new().unwrap();
            let wt = TempDir::new().unwrap();
            let rig = Rig::calm();
            let ask = |limit, seed| CorrectnessAsk {
                tool: Some(HeavyTool::Gradle),
                cwd: wt.path(),
                max_unity_parallel: 1,
                wait_limit_secs: Some(limit),
                jitter_seed: seed,
            };
            let _held = acquire_correctness(data.path(), &ask(0, 1), &*rig.clock, &rig.sleeper)
                .unwrap()
                .unwrap();
            let time = MillisTime {
                millis: std::sync::atomic::AtomicU64::new(start_millis),
                slept: std::sync::Mutex::new(Vec::new()),
            };
            let got = acquire_correctness(data.path(), &ask(limit, 2), &time, &time).unwrap();
            assert!(got.is_none());
            let slept: Duration = time.slept.lock().unwrap().iter().sum();
            assert!(
                slept >= Duration::from_secs(limit),
                "limit {limit}: gave up after {slept:?} (start {start_millis} ms)"
            );
            assert!(
                slept <= Duration::from_millis(max_millis),
                "limit {limit}: overshot to {slept:?} (start {start_millis} ms)"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn retry_loop_rejects_pools_dir_swapped_with_symlink_between_polls() {
        use std::os::unix::fs::symlink;
        use std::sync::{Arc, Mutex};

        let data = TempDir::new().unwrap();
        let wt = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let rig = Rig::calm();

        let holder = acquire_correctness(
            data.path(),
            &CorrectnessAsk {
                tool: Some(HeavyTool::Gradle),
                cwd: wt.path(),
                max_unity_parallel: 1,
                wait_limit_secs: Some(0),
                jitter_seed: 1,
            },
            &*rig.clock,
            &rig.sleeper,
        )
        .unwrap()
        .unwrap();
        let holder = Arc::new(Mutex::new(Some(holder)));
        let holder_ref = Arc::clone(&holder);
        let pools = super::super::paths::pools_dir(data.path());
        let outside_path = outside.path().to_path_buf();
        rig.sleeper.on_sleep(move |_| {
            holder_ref.lock().unwrap().take();
            fs::remove_dir_all(&pools).unwrap();
            symlink(&outside_path, &pools).unwrap();
        });

        let err = acquire_correctness(
            data.path(),
            &CorrectnessAsk {
                tool: Some(HeavyTool::Gradle),
                cwd: wt.path(),
                max_unity_parallel: 1,
                wait_limit_secs: Some(5),
                jitter_seed: 2,
            },
            &*rig.clock,
            &rig.sleeper,
        );
        assert!(
            err.is_err(),
            "swapped pools/ symlink must fail closed on retry"
        );
    }
}
