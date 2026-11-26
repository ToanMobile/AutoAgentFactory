#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Regression tests for throttle hardening:
//! - I-5a: sandboxed guests inherit host `heavy_stats.json` via read-only
//!   `throttle/stats.json` mirror and cannot spoof reservations downward.
//! - I-1a: gate `idle_timeout_secs` terminates silent/GC-thrashing commands
//!   early with a diagnostic note, while resetting on output.
//! - I-3: `aaf slot` aborts and drops `drain.lock` if its parent process exits
//!   while waiting in the throttle queue.
//! - Defense-in-depth: `max_gradle_parallel` caps concurrent Gradle builds.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use aaf_domain::config::{CommandsConfig, MachineConfig};
use aaf_domain::gate::GateMode;
use aaf_domain::ports::Clock;
use aaf_engine::gate::CommandTimeouts;
use aaf_engine::gate::runner::{self, HeavyContext};
use aaf_engine::throttle::pools::{self, CorrectnessAsk};
use aaf_engine::throttle::stats::{self, HeavySample};
use aaf_engine::throttle::{
    GroupControl, HeavyTool, LoadProbe, Pressure, PriorityTools, Sleeper, SlotJob, SystemGroup,
    Throttle,
};
use tempfile::TempDir;

struct FixedClock(AtomicI64);
impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct CalmProbe;
impl LoadProbe for CalmProbe {
    fn pressure(&self) -> Option<Pressure> {
        Some(Pressure::Normal)
    }
    fn load_percent(&self) -> Option<u32> {
        Some(10)
    }
    fn free_memory_bytes(&self) -> Option<u64> {
        Some(16 * 1024 * 1024 * 1024)
    }
}

struct FastSleeper;
impl Sleeper for FastSleeper {
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d.min(Duration::from_millis(5)));
    }
}

#[test]
fn guest_stats_inherits_host_mirror_and_rejects_downgrade_spoof() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path();
    let sleeper = FastSleeper;

    // 1. Host records a 6 GiB Gradle run -> automatically mirrored to throttle/stats.json.
    let recorded = stats::record(
        data,
        "repo1234",
        "@tool:gradle",
        HeavySample {
            peak_rss_bytes: 6_000_000_000,
            duration_ms: 45_000,
        },
        &sleeper,
    )
    .unwrap();
    assert!(recorded);
    assert!(stats::host_mirror_path(data).is_file());

    // 2. Simulate sandboxed guest: make throttle/drain.lock read-only so `is_guest` is true.
    let throttle_dir = data.join("throttle");
    fs::create_dir_all(throttle_dir.join("guest")).unwrap();
    let drain = throttle_dir.join("drain.lock");
    fs::write(&drain, b"").unwrap();
    fs::set_permissions(&drain, fs::Permissions::from_mode(0o444)).unwrap();

    // Guest has no guest/heavy_stats.json yet, yet inherits host mirror's 6 GiB peak!
    let peak = stats::recorded_peak_for(
        data,
        "repo1234",
        "custom-unit-test",
        Some(HeavyTool::Gradle),
        None,
    );
    assert_eq!(peak, Some(6_000_000_000));

    // 3. Malicious guest tries to spoof a tiny 1 MiB peak in guest/heavy_stats.json:
    // host mirror's 6 GiB must win via max(host, guest).
    fs::write(
        throttle_dir.join("guest/heavy_stats.json"),
        r#"{"entries":{"repo1234/custom-unit-test":[{"peak_rss_bytes":1048576,"duration_ms":100}]}}"#,
    )
    .unwrap();
    let peak_after_spoof = stats::recorded_peak_for(
        data,
        "repo1234",
        "custom-unit-test",
        Some(HeavyTool::Gradle),
        None,
    );
    assert_eq!(peak_after_spoof, Some(6_000_000_000));

    // Restore write permissions for TempDir cleanup.
    fs::set_permissions(&drain, fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn gate_idle_timeout_kills_silent_command_and_resets_on_output() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let data = tmp.path().join("data");
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir_all(&data).unwrap();

    let machine = MachineConfig::default();
    let probe = CalmProbe;
    let sleeper = FastSleeper;
    let control = SystemGroup;
    let progress = |_: &aaf_engine::throttle::Progress<'_>| {};
    let throttle = Throttle {
        machine: &machine,
        probe: &probe,
        sleeper: &sleeper,
        control: &control,
        progress: &progress,
        cpus: 4,
        tools: PriorityTools::default(),
        jitter_seed: 1,
        sample_every: Duration::from_secs(60),
    };
    let clock = FixedClock(AtomicI64::new(1_000));
    let heavy = HeavyContext {
        data_dir: &data,
        stats_repo_id: "repo_idle_test",
        clock: &clock,
        throttle: &throttle,
    };

    // 1. Silent command (wall 10s, idle 1s) must be killed well before wall timeout.
    let cmds = CommandsConfig {
        test_scoped: "sleep 10".to_string(),
        ..Default::default()
    };
    let started = Instant::now();
    let run = runner::run_all_throttled_with_timeouts(
        &repo,
        &cmds,
        GateMode::Scoped,
        CommandTimeouts {
            wall: Duration::from_secs(10),
            idle: Some(Duration::from_secs(1)),
        },
        &heavy,
        &[],
        &[],
    )
    .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "idle timeout should cut silent command well before 10s wall timeout"
    );
    assert_eq!(run.results.len(), 1);
    assert!(run.results[0].timed_out);
    assert!(
        run.results[0].log_tail.contains("idle_timeout_secs"),
        "expected diagnostic note, got: {}",
        run.results[0].log_tail
    );

    // 2. Chatty command emitting output every 300ms for 1.5s survives a 1s idle timeout.
    let chatty = CommandsConfig {
        test_scoped: "i=0; while [ $i -lt 5 ]; do echo tick-$i; sleep 0.3; i=$((i+1)); done"
            .to_string(),
        ..Default::default()
    };
    let run_ok = runner::run_all_throttled_with_timeouts(
        &repo,
        &chatty,
        GateMode::Scoped,
        CommandTimeouts {
            wall: Duration::from_secs(10),
            idle: Some(Duration::from_secs(1)),
        },
        &heavy,
        &[],
        &[],
    )
    .unwrap();
    assert!(!run_ok.results[0].timed_out);
    assert_eq!(run_ok.results[0].exit_code, Some(0));
}

#[test]
fn gradle_pool_caps_concurrent_gradle_at_max_gradle_parallel() {
    let data = TempDir::new().unwrap();
    let wt1 = TempDir::new().unwrap();
    let wt2 = TempDir::new().unwrap();
    let wt3 = TempDir::new().unwrap();
    let clock = FixedClock(AtomicI64::new(1_000));
    let sleeper = FastSleeper;

    // Two distinct Gradle repos fit when max_gradle_parallel = 2, third is refused at wait_limit=0.
    let machine = MachineConfig {
        exclusive_build_tools: false,
        max_gradle_parallel: 2,
        ..Default::default()
    };
    let probe = CalmProbe;
    let control = SystemGroup;
    let progress = |_: &aaf_engine::throttle::Progress<'_>| {};
    let throttle = Throttle {
        machine: &machine,
        probe: &probe,
        sleeper: &sleeper,
        control: &control,
        progress: &progress,
        cpus: 12,
        tools: PriorityTools::default(),
        jitter_seed: 7,
        sample_every: Duration::from_secs(60),
    };

    // Hold tool.gradle.0.lock and tool.gradle.1.lock directly via pools dir.
    let pools = aaf_engine::throttle::paths::pools_dir(data.path());
    fs::create_dir_all(&pools).unwrap();
    let f0 = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(pools.join("tool.gradle.0.lock"))
        .unwrap();
    f0.try_lock().unwrap();
    let f1 = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(pools.join("tool.gradle.1.lock"))
        .unwrap();
    f1.try_lock().unwrap();

    // A 3rd Gradle invocation (wt3) with wait_limit_secs = Some(0) must return Ok(None).
    let job = SlotJob {
        repo_id: "r3",
        label: "gradlew",
        argv: &[OsString::from("./gradlew"), OsString::from("test")],
        cwd: wt3.path(),
        wait_limit_secs: Some(0),
        exec_timeout: None,
        inherit_stdin: false,
    };
    let got = throttle.run_slot(data.path(), &clock, &job).unwrap();
    assert!(got.is_none(), "3rd concurrent Gradle must be capped at 2");

    // Releasing one lock lets a non-Gradle or Gradle job pass Layer-1 cleanly.
    drop(f1);
    let ask_wt1 = CorrectnessAsk {
        tool: Some(HeavyTool::Gradle),
        cwd: wt1.path(),
        max_unity_parallel: 1,
        wait_limit_secs: Some(0),
        jitter_seed: 1,
    };
    let g1 = pools::acquire_correctness(data.path(), &ask_wt1, &clock, &sleeper).unwrap();
    assert!(g1.is_some());
    drop((f0, g1, wt2));
}

#[test]
fn slot_wait_aborts_immediately_when_watched_parent_pid_differs() {
    let data = TempDir::new().unwrap();
    let wt = TempDir::new().unwrap();
    let clock = FixedClock(AtomicI64::new(1_000));
    let sleeper = FastSleeper;
    let probe = CalmProbe;
    let control = SystemGroup;
    let progress = |_: &aaf_engine::throttle::Progress<'_>| {};
    let machine = MachineConfig::default();
    let throttle = Throttle {
        machine: &machine,
        probe: &probe,
        sleeper: &sleeper,
        control: &control,
        progress: &progress,
        cpus: 4,
        tools: PriorityTools::default(),
        jitter_seed: 3,
        sample_every: Duration::from_secs(60),
    };

    // Occupy heavy.0.lock so the waiter must enter the Phase B wait loop.
    aaf_engine::throttle::slots::prepare_for_guests(data.path(), 1, &|pgid| {
        control.group_state(pgid)
    })
    .unwrap();
    let held = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(aaf_engine::throttle::paths::slot_path(data.path(), 0))
        .unwrap();
    held.try_lock().unwrap();

    // Pass a watched parent PID that does NOT match our real parent_id() (simulating reparenting).
    let fake_dead_parent = std::os::unix::process::parent_id()
        .wrapping_add(12345)
        .max(2);
    let job = SlotJob {
        repo_id: "orphan-test",
        label: "true",
        argv: &[OsString::from("/usr/bin/true")],
        cwd: wt.path(),
        wait_limit_secs: None, // unlimited wait, just like PID 17934!
        exec_timeout: None,
        inherit_stdin: false,
    };
    let started = Instant::now();
    let res = throttle
        .run_slot_watching_parent(data.path(), &clock, &job, Some(fake_dead_parent))
        .unwrap();
    assert!(
        res.is_none(),
        "orphaned waiter must abort instead of hanging forever"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "orphan abort must be immediate"
    );
    // Verify drain.lock is NOT left locked.
    let drain = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(aaf_engine::throttle::paths::drain_path(data.path()))
        .unwrap();
    assert!(
        drain.try_lock().is_ok(),
        "drain.lock must be free after abort"
    );
    drop(held);
}
