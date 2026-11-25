#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use aaf_domain::config::MachineConfig;
use aaf_domain::ports::Clock;
use aaf_engine::throttle::probe::{GroupState, LoadProbe, Pressure, ProcId};
use aaf_engine::throttle::slots::{Admission, MachineView, try_acquire_in};
use aaf_engine::throttle::stats;
use aaf_engine::throttle::watchdog::{GroupControl, PriorityTools, TreeSample};
use aaf_engine::throttle::{HeavyJob, HeavySample, Progress, Sleeper, Throttle};
use tempfile::TempDir;

const REPO_ID: &str = "oracle-res040";
const LABEL: &str = "test_scoped";
const LEADER_STARTED: &str = "Tue Nov 24 12:00:00 2026";
const HELPER_STARTED: &str = "Tue Nov 24 12:00:01 2026";
const HELPER_PID: u32 = 999_040;

#[derive(Clone, Copy)]
enum ReleaseTrigger {
    /// Return pressure to `Normal` after stop; release child inside `resume()`.
    OnResume,
    /// Keep pressure `Critical` and release child on `-STOP` so `Watchdog::finish()` runs `resume()`.
    OnStop,
}

struct FakeMachine {
    watchdog_active: AtomicBool,
    helper_stopped: AtomicBool,
    stop_calls: AtomicUsize,
    resume_calls: AtomicUsize,
    signals: Mutex<Vec<(String, Vec<ProcId>)>>,
    helper: ProcId,
    trigger: ReleaseTrigger,
    release_file: PathBuf,
}

impl Clock for FakeMachine {
    fn now(&self) -> i64 {
        1_000
    }
}

impl Sleeper for FakeMachine {
    fn sleep(&self, _duration: Duration) {}
}

impl LoadProbe for FakeMachine {
    fn load_percent(&self) -> Option<u32> {
        Some(0)
    }

    fn pressure(&self) -> Option<Pressure> {
        if !self.watchdog_active.load(Ordering::SeqCst) {
            return Some(Pressure::Normal);
        }
        match self.trigger {
            ReleaseTrigger::OnResume if self.helper_stopped.load(Ordering::SeqCst) => {
                Some(Pressure::Normal)
            }
            ReleaseTrigger::OnResume | ReleaseTrigger::OnStop => Some(Pressure::Critical),
        }
    }

    fn free_memory_bytes(&self) -> Option<u64> {
        Some(u64::MAX)
    }
}

impl GroupControl for FakeMachine {
    fn rss_bytes(&self, _pgid: u32) -> Option<u64> {
        Some(4096)
    }

    fn stop(&self, _pgid: u32) -> bool {
        self.stop_calls.fetch_add(1, Ordering::SeqCst);
        true
    }

    fn resume(&self, _pgid: u32) -> bool {
        self.resume_calls.fetch_add(1, Ordering::SeqCst);
        if matches!(self.trigger, ReleaseTrigger::OnResume) {
            let _ = fs::write(&self.release_file, b"go\n");
        }
        // Simulate ESRCH / dead main process group.
        false
    }

    fn group_state(&self, _pgid: u32) -> Option<GroupState> {
        (self.resume_calls.load(Ordering::SeqCst) == 0).then(|| GroupState {
            stopped: self.helper_stopped.load(Ordering::SeqCst),
            leader_started: Some(LEADER_STARTED.to_string()),
        })
    }

    fn tree(&self, pgid: u32, _known: &[ProcId]) -> Option<TreeSample> {
        self.watchdog_active.store(true, Ordering::SeqCst);
        let mut members = Vec::new();
        if !self.helper_stopped.load(Ordering::SeqCst) {
            members.push(ProcId {
                pid: pgid,
                started: LEADER_STARTED.to_string(),
            });
        }
        members.push(self.helper.clone());
        Some(TreeSample {
            rss_bytes: 4096,
            members,
        })
    }

    fn signal_members(&self, signal: &str, members: &[ProcId]) -> usize {
        self.signals
            .lock()
            .unwrap()
            .push((signal.to_string(), members.to_vec()));
        if signal == "-STOP" && members.contains(&self.helper) {
            self.helper_stopped.store(true, Ordering::SeqCst);
            if matches!(self.trigger, ReleaseTrigger::OnStop) {
                let _ = fs::write(&self.release_file, b"go\n");
            }
        }
        members.len()
    }
}

fn run_res040_scenario(trigger: ReleaseTrigger) {
    let data = TempDir::new().unwrap();
    let release_file = data.path().join("release_child");
    let helper = ProcId {
        pid: HELPER_PID,
        started: HELPER_STARTED.to_string(),
    };
    let fake = FakeMachine {
        watchdog_active: AtomicBool::new(false),
        helper_stopped: AtomicBool::new(false),
        stop_calls: AtomicUsize::new(0),
        resume_calls: AtomicUsize::new(0),
        signals: Mutex::new(Vec::new()),
        helper: helper.clone(),
        trigger,
        release_file: release_file.clone(),
    };

    // Pre-record a measurement so the command shares slot 1 alongside slot 0.
    let sample = HeavySample {
        peak_rss_bytes: 1,
        duration_ms: 10,
    };
    stats::record(data.path(), REPO_ID, LABEL, sample, &fake).unwrap();

    let view = MachineView {
        free_memory: &|| Some(u64::MAX),
        group_rss: &|_| None,
        group_state: &|_| None,
    };
    let adm = Admission::Measured { peak_rss_bytes: 1 };
    let _slot0 = try_acquire_in(data.path(), 2, adm, &view).unwrap().unwrap();
    assert_eq!(_slot0.index(), 0);

    let quiet = |_: &Progress<'_>| {};
    let machine = MachineConfig {
        max_heavy_parallel: 2,
        max_load_percent: 0,
        low_priority: false,
        exclusive_build_tools: true,
        ..MachineConfig::default()
    };
    let throttle = Throttle {
        machine: &machine,
        probe: &fake,
        sleeper: &fake,
        control: &fake,
        progress: &quiet,
        cpus: 4,
        tools: PriorityTools::default(),
        jitter_seed: 1,
        sample_every: Duration::from_millis(10),
    };
    let job = HeavyJob {
        repo_id: REPO_ID,
        label: LABEL,
        command: r#"i=0; while [ ! -f "$AAF_RES040_DONE" ] && [ "$i" -lt 200 ]; do /bin/sleep 0.01; i=$((i+1)); done"#,
        cwd: data.path(),
        timeout: Duration::from_secs(10),
        log_tail_bytes: 1024,
    };
    let env = [("AAF_RES040_DONE", release_file.to_str().unwrap())];
    let run = throttle
        .run_heavy_with_env(data.path(), &fake, &job, &env)
        .unwrap();

    assert_eq!(run.slot, 1, "expected command to run in slot 1");
    assert_eq!(run.exit_code, Some(0), "child command should exit cleanly");
    assert!(!run.timed_out, "child command should not time out");
    assert!(fake.stop_calls.load(Ordering::SeqCst) >= 1, "expected stop");
    assert!(
        fake.resume_calls.load(Ordering::SeqCst) >= 1,
        "expected resume"
    );

    let signals = fake.signals.lock().unwrap();
    let stop_idx = signals
        .iter()
        .position(|(sig, members)| sig == "-STOP" && members.contains(&helper))
        .expect("expected Watchdog::stop to SIGSTOP the tracked helper");
    let cont_after_stop = signals[stop_idx + 1..]
        .iter()
        .any(|(sig, members)| sig == "-CONT" && members.contains(&helper));
    assert!(
        cont_after_stop,
        "RES-040: helper {:?} was SIGSTOPped by Watchdog::stop and must receive SIGCONT via \
         signal_members(\"-CONT\", ...) even when control.resume(pgid) returns false; \
         observed signals: {:?}",
        helper, *signals
    );
}

#[test]
fn res040_normal_pressure_continues_stopped_helper_when_group_resume_fails() {
    run_res040_scenario(ReleaseTrigger::OnResume);
}

#[test]
fn res040_finish_continues_stopped_helper_when_group_resume_fails() {
    run_res040_scenario(ReleaseTrigger::OnStop);
}
