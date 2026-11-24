//! One `--timeout` for both layers: the Layer-1 correctness wait and the
//! Layer-2 slot wait share a millisecond budget (audit a13 L1-R2-1; live:
//! `aaf slot --timeout 3` behind a worktree lock gave up after 3.82 s).

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aaf_domain::ports::Clock;
use tempfile::TempDir;

use super::super::heavy::HeavyTool;
use super::super::pools::{CorrectnessAsk, acquire_correctness};
use super::super::run::Inherited;
use super::super::slots::{Admission, try_acquire};
use super::super::stats::{HeavySample, record};
use super::super::{Sleeper, SlotJob};
use super::support::{GIB, Rig};

type Hook = Box<dyn FnMut(usize) + Send>;

/// A clock and sleeper that keep milliseconds; the clock reads whole
/// seconds like the real one. `hook` runs after each sleep with its count.
struct MillisTime {
    millis: AtomicU64,
    slept: Mutex<Vec<Duration>>,
    hook: Mutex<Option<Hook>>,
}

impl Clock for MillisTime {
    fn now(&self) -> i64 {
        i64::try_from(self.millis.load(Ordering::SeqCst) / 1_000).unwrap()
    }
}

impl Sleeper for MillisTime {
    fn sleep(&self, duration: Duration) {
        let count = {
            let mut slept = self.slept.lock().unwrap();
            slept.push(duration);
            slept.len()
        };
        let millis = u64::try_from(duration.as_millis()).unwrap();
        self.millis.fetch_add(millis, Ordering::SeqCst);
        if let Some(hook) = self.hook.lock().unwrap().as_mut() {
            hook(count);
        }
    }
}

#[test]
fn both_layers_together_keep_the_limit_to_the_millisecond() {
    for start_millis in [1_000_000u64, 1_000_999] {
        let data = TempDir::new().unwrap();
        let work = TempDir::new().unwrap();
        let tool = work.path().join("gradlew");
        fs::write(&tool, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        let mut rig = Rig::calm();
        rig.machine.max_heavy_parallel = 1;
        rig.machine.exclusive_build_tools = false;
        let sample = HeavySample {
            peak_rss_bytes: 1024,
            duration_ms: 1,
        };
        record(data.path(), "dir", "job", sample, &rig.sleeper).unwrap();
        // Layer 2 stays busy for the whole wait.
        let small = Admission::Measured { peak_rss_bytes: 1 };
        let _slot = try_acquire(data.path(), 1, small, &|| Some(64 * GIB))
            .unwrap()
            .unwrap();
        // Layer 1 frees after three backoff sleeps (about 1.75 s).
        let lock = acquire_correctness(
            data.path(),
            &CorrectnessAsk {
                tool: Some(HeavyTool::Gradle),
                cwd: work.path(),
                max_unity_parallel: 1,
                wait_limit_secs: Some(0),
                jitter_seed: 1,
            },
            &*rig.clock,
            &rig.sleeper,
        )
        .unwrap()
        .unwrap();
        let lock = Arc::new(Mutex::new(Some(lock)));
        let release = Arc::clone(&lock);
        let time = MillisTime {
            millis: AtomicU64::new(start_millis),
            slept: Mutex::new(Vec::new()),
            hook: Mutex::new(Some(Box::new(move |count| {
                if count == 3 {
                    release.lock().unwrap().take();
                }
            }))),
        };
        let mut throttle = rig.throttle();
        throttle.sleeper = &time;
        let command: Vec<OsString> = ["./gradlew", "lint"].iter().map(OsString::from).collect();
        let job = SlotJob {
            repo_id: "dir",
            label: "job",
            argv: &command,
            cwd: work.path(),
            wait_limit_secs: Some(3),
            exec_timeout: None,
            inherit_stdin: false,
        };
        let got = throttle
            .run_slot_with(data.path(), &time, &job, &Inherited::default())
            .unwrap();
        assert!(got.is_none(), "start {start_millis} ms");
        assert!(lock.lock().unwrap().is_none(), "Layer 1 was passed");
        let slept: Duration = time.slept.lock().unwrap().iter().sum();
        assert!(
            slept >= Duration::from_secs(3),
            "gave up after {slept:?} (start {start_millis} ms)"
        );
        assert!(
            slept <= Duration::from_millis(3_250),
            "overshot to {slept:?} (start {start_millis} ms)"
        );
    }
}
