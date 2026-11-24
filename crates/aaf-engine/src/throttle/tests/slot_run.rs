//! `Throttle::run_slot`: any command through the queue of the heavy gate
//! commands, with real small children.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::sync::{Arc, Mutex};

use tempfile::TempDir;

use super::super::Throttle;
use super::super::run::Inherited;
use super::super::slots::{Admission, is_held, try_acquire};
use super::super::stats::{HeavySample, record, recorded_peak};
use super::super::token::SlotToken;
use super::super::{SlotJob, SlotRun};
use super::support::{GIB, MarksSampled, Rig, after_file};

const REPO: &str = "dir";
const SMALL: Admission = Admission::Measured { peak_rss_bytes: 1 };

fn plenty() -> Option<u64> {
    Some(64 * GIB)
}

#[test]
fn slot_reads_as_held_only_while_somebody_holds_it() {
    let data = TempDir::new().unwrap();
    assert!(!is_held(data.path(), 0));
    let held = try_acquire(data.path(), 2, SMALL, &plenty)
        .unwrap()
        .unwrap();
    assert!(is_held(data.path(), 0));
    // Looking takes nothing: the slot beside it is still free.
    assert!(!is_held(data.path(), 1));
    assert!(is_held(data.path(), 0));
    drop(held);
    assert!(!is_held(data.path(), 0));
}

fn argv(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

fn job<'a>(cwd: &'a Path, argv: &'a [OsString], wait_limit_secs: Option<u64>) -> SlotJob<'a> {
    SlotJob {
        repo_id: REPO,
        label: "job",
        argv,
        cwd,
        wait_limit_secs,
        exec_timeout: None,
        inherit_stdin: false,
    }
}

fn run(rig: &Rig, data: &Path, job: &SlotJob<'_>) -> Option<SlotRun> {
    rig.throttle()
        .run_slot_with(data, &*rig.clock, job, &Inherited::default())
        .unwrap()
}

#[test]
fn exit_status_of_the_command_comes_back_as_it_is() {
    let data = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let rig = Rig::calm();

    let passed = run(&rig, data.path(), &job(work.path(), &argv(&["true"]), None)).unwrap();
    assert_eq!(passed.status.code(), Some(0));
    assert_eq!((passed.slot, passed.waited_secs), (0, 0));

    let failing = argv(&["sh", "-c", "exit 3"]);
    let failed = run(&rig, data.path(), &job(work.path(), &failing, None)).unwrap();
    assert_eq!(failed.status.code(), Some(3));

    let dying = argv(&["sh", "-c", "kill -TERM $$"]);
    let killed = run(&rig, data.path(), &job(work.path(), &dying, None)).unwrap();
    assert_eq!(
        (killed.status.code(), killed.status.signal()),
        (None, Some(15))
    );
}

#[test]
fn command_runs_in_its_directory_with_the_daemon_switches_and_its_slot() {
    let data = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let rig = Rig::calm();
    let script = argv(&[
        "sh",
        "-c",
        "printf '%s|%s' \"$GRADLE_OPTS\" \"$AAF_HEAVY_SLOT\" > seen.txt",
    ]);
    let done = run(&rig, data.path(), &job(work.path(), &script, None)).unwrap();
    assert!(done.status.success());
    let seen = fs::read_to_string(work.path().join("seen.txt")).unwrap();
    // The slot, and the token that proves it to an `aaf slot` further down.
    let (opts, slot) = seen.split_once('|').unwrap();
    assert!(opts.starts_with("-Dorg.gradle.daemon=false "), "{seen}");
    let token = slot.strip_prefix("0:").and_then(SlotToken::parse);
    assert!(token.is_some(), "{seen}");
}

#[test]
fn gradle_under_a_path_with_a_space_runs_alone_too() {
    let data = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let gradlew = work.path().join("my app/gradlew");
    fs::create_dir_all(gradlew.parent().unwrap()).unwrap();
    fs::write(&gradlew, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&gradlew, fs::Permissions::from_mode(0o755)).unwrap();
    let mut rig = Rig::calm();
    rig.machine.max_heavy_parallel = 2;
    let sample = HeavySample {
        peak_rss_bytes: 1024,
        duration_ms: 1,
    };
    record(data.path(), REPO, "job", sample, &rig.sleeper).unwrap();
    // A small command runs in slot 0 until the first poll.
    let holder = try_acquire(data.path(), 2, SMALL, &plenty)
        .unwrap()
        .unwrap();
    let holder = Arc::new(Mutex::new(Some(holder)));
    let release = Arc::clone(&holder);
    rig.sleeper.on_sleep(move |_| {
        release.lock().unwrap().take();
    });

    let command = vec![gradlew.into_os_string(), OsString::from("lint")];
    let done = run(&rig, data.path(), &job(work.path(), &command, None)).unwrap();
    assert!(done.status.success());
    // It waited for the machine to itself instead of taking slot 1.
    assert_eq!((done.slot, rig.sleeper.count()), (0, 1));
}

#[test]
fn wait_limit_ends_the_wait_and_the_command_is_never_started() {
    let data = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let mut rig = Rig::calm();
    rig.machine.max_heavy_parallel = 1;
    let held = try_acquire(data.path(), 1, SMALL, &plenty)
        .unwrap()
        .unwrap();
    let touch = argv(&["touch", "ran.mark"]);

    // No waiting at all, then a wait that ends on the injected clock.
    for limit in [0, 30] {
        let before = rig.sleeper.count();
        let outcome = run(&rig, data.path(), &job(work.path(), &touch, Some(limit)));
        assert_eq!(outcome, None, "limit {limit}");
        assert_eq!(rig.sleeper.count() > before, limit > 0, "limit {limit}");
        assert!(!work.path().join("ran.mark").exists());
    }

    // The same command with the slot free.
    drop(held);
    let outcome = run(&rig, data.path(), &job(work.path(), &touch, Some(0)));
    assert!(outcome.unwrap().status.success());
    assert!(work.path().join("ran.mark").exists());
}

#[test]
fn passing_command_is_measured_under_its_label_and_then_shares_the_machine() {
    let data = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let mut rig = Rig::calm();
    rig.real_group = true;
    let nap = argv(&["sleep", "0.3"]);

    // The first command waits until it has been sampled: under load a
    // `sleep 0.3` had ended before the first sample, which read 0 bytes.
    let control = MarksSampled {
        file: work.path().join("sampled"),
    };
    let measured = argv(&["/bin/sh", "-c", &after_file(&control.file, "exit 0")]);
    let first = Throttle {
        control: &control,
        ..rig.throttle()
    }
    .run_slot_with(
        data.path(),
        &*rig.clock,
        &job(work.path(), &measured, None),
        &Inherited::default(),
    )
    .unwrap()
    .unwrap();
    assert!(first.status.success());
    let peak = recorded_peak(data.path(), REPO, "job");
    assert!(peak.is_some_and(|peak| peak > 0), "{peak:?}");

    // Measured now: it takes the second slot beside a running command.
    let _held = try_acquire(data.path(), 2, SMALL, &plenty)
        .unwrap()
        .unwrap();
    let second = run(&rig, data.path(), &job(work.path(), &nap, None)).unwrap();
    assert_eq!((second.slot, second.waited_secs), (1, 0));
}

#[test]
fn gradle_given_as_an_argument_vector_runs_alone_too() {
    let data = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let gradlew = work.path().join("gradlew");
    fs::write(&gradlew, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&gradlew, fs::Permissions::from_mode(0o755)).unwrap();
    let rig = Rig::calm();
    let sample = HeavySample {
        peak_rss_bytes: 1024,
        duration_ms: 1,
    };
    record(data.path(), REPO, "job", sample, &rig.sleeper).unwrap();
    let holder = try_acquire(data.path(), 2, SMALL, &plenty)
        .unwrap()
        .unwrap();
    let holder = Arc::new(Mutex::new(Some(holder)));
    let release = Arc::clone(&holder);
    rig.sleeper.on_sleep(move |_| {
        release.lock().unwrap().take();
    });

    let command = argv(&["./gradlew", "lint"]);
    let done = run(&rig, data.path(), &job(work.path(), &command, None)).unwrap();
    assert!(done.status.success());
    assert_eq!((done.slot, rig.sleeper.count()), (0, 1));
}

#[test]
fn empty_command_is_an_error_and_takes_no_slot() {
    let data = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let rig = Rig::calm();
    let outcome = rig.throttle().run_slot_with(
        data.path(),
        &*rig.clock,
        &job(work.path(), &[], None),
        &Inherited::default(),
    );
    assert_eq!(
        outcome.unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );
    assert!(
        try_acquire(data.path(), 1, Admission::Unmeasured, &plenty)
            .unwrap()
            .is_ok()
    );
}

#[test]
fn measured_build_tool_shares_slots_when_exclusive_build_tools_is_false() {
    let data = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let gradlew = work.path().join("gradlew");
    fs::write(&gradlew, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&gradlew, fs::Permissions::from_mode(0o755)).unwrap();
    let mut rig = Rig::calm();
    rig.machine.max_heavy_parallel = 2;
    rig.machine.exclusive_build_tools = false;
    let sample = HeavySample {
        peak_rss_bytes: 1024,
        duration_ms: 1,
    };
    record(data.path(), REPO, "job", sample, &rig.sleeper).unwrap();
    // Slot 0 stays held throughout; a measured build tool must take slot 1 immediately.
    let _holder = try_acquire(data.path(), 2, SMALL, &plenty)
        .unwrap()
        .unwrap();

    let command = argv(&["./gradlew", "lint"]);
    let done = run(&rig, data.path(), &job(work.path(), &command, Some(1)));
    assert!(
        done.is_some(),
        "measured build tool should share slot 1 when exclusive_build_tools = false"
    );
    let done = done.unwrap();
    assert!(done.status.success());
    assert_eq!((done.slot, rig.sleeper.count()), (1, 0));

    // Unmeasured build tools still run alone even when exclusive_build_tools = false.
    let unmeasured_job = SlotJob {
        repo_id: REPO,
        label: "never-measured",
        argv: &command,
        cwd: work.path(),
        wait_limit_secs: Some(1),
        exec_timeout: None,
        inherit_stdin: false,
    };
    assert!(run(&rig, data.path(), &unmeasured_job).is_none());
}

#[test]
fn layer1_wait_counts_toward_waited_secs_and_subtracts_from_layer2_wait_limit() {
    use super::super::heavy::HeavyTool;
    use super::super::pools::{CorrectnessAsk, acquire_correctness};

    let data = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let gradlew = work.path().join("gradlew");
    fs::write(&gradlew, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&gradlew, fs::Permissions::from_mode(0o755)).unwrap();
    let mut rig = Rig::calm();
    rig.machine.max_heavy_parallel = 1;
    rig.machine.exclusive_build_tools = false;
    let sample = HeavySample {
        peak_rss_bytes: 1024,
        duration_ms: 1,
    };
    record(data.path(), REPO, "job", sample, &rig.sleeper).unwrap();

    // Part 1: Layer 1 waits 2 polls, then Layer 2 admits immediately -> `waited_secs` includes Layer 1.
    let wt_lock = acquire_correctness(
        data.path(),
        &CorrectnessAsk {
            tool: Some(HeavyTool::Gradle),
            cwd: work.path(),
            max_unity_parallel: 1,
            wait_limit_secs: None,
            jitter_seed: 1,
        },
        &*rig.clock,
        &rig.sleeper,
    )
    .unwrap()
    .unwrap();
    let wt_lock = Arc::new(Mutex::new(Some(wt_lock)));
    let release = Arc::clone(&wt_lock);
    rig.sleeper.on_sleep(move |count| {
        if count == 2 {
            release.lock().unwrap().take();
        }
    });
    let command = argv(&["./gradlew", "lint"]);
    let done = run(&rig, data.path(), &job(work.path(), &command, Some(10))).unwrap();
    assert!(done.status.success());
    assert!(
        done.waited_secs >= 2,
        "expected waited_secs to include Layer-1 wait, got {}",
        done.waited_secs
    );

    // Part 2: Layer 1 spends part of the wait budget (3 s) while Layer 2 slot 0 is busy ->
    // Layer 2 spends what is left of the same budget and no more. (This fake clock moves a
    // whole second per sleep; the budget is kept to the millisecond of what was slept, see
    // `layer1_budget.rs`, so Layer 2 may still sleep the remainder.)
    let work2 = TempDir::new().unwrap();
    let gradlew2 = work2.path().join("gradlew");
    fs::write(&gradlew2, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&gradlew2, fs::Permissions::from_mode(0o755)).unwrap();
    let rig2 = {
        let mut r = Rig::calm();
        r.machine.max_heavy_parallel = 1;
        r.machine.exclusive_build_tools = false;
        r
    };
    let _slot0 = try_acquire(data.path(), 1, SMALL, &plenty)
        .unwrap()
        .unwrap();
    let wt_lock2 = acquire_correctness(
        data.path(),
        &CorrectnessAsk {
            tool: Some(HeavyTool::Gradle),
            cwd: work2.path(),
            max_unity_parallel: 1,
            wait_limit_secs: Some(0),
            jitter_seed: 2,
        },
        &*rig2.clock,
        &rig2.sleeper,
    )
    .unwrap()
    .unwrap();
    let wt_lock2 = Arc::new(Mutex::new(Some(wt_lock2)));
    let release2 = Arc::clone(&wt_lock2);
    rig2.sleeper.on_sleep(move |count| {
        if count == 3 {
            release2.lock().unwrap().take();
        }
    });
    let outcome = run(&rig2, data.path(), &job(work2.path(), &command, Some(3)));
    assert_eq!(outcome, None);
    assert!(rig2.sleeper.count() > 3, "Layer 1 was passed");
    let slept: std::time::Duration = rig2.sleeper.slept.lock().unwrap().iter().sum();
    assert!(
        slept <= std::time::Duration::from_secs(3),
        "Layer 2 must not sleep past the remaining budget after Layer 1: {slept:?}"
    );
}

/// A clock whose every reading is one second later than the one before: the
/// worst case of a real clock crossing a second between two readings.
struct TickingClock(std::sync::atomic::AtomicI64);

impl aaf_domain::ports::Clock for TickingClock {
    fn now(&self) -> i64 {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    }
}

/// Neither layer slept, so nothing was waited: a second the clock crossed
/// between two readings is not a wait (audit a13: the Layer-1 time was
/// counted even when no correctness lock was waited for, reintroducing the
/// "waited 1 s" flake fixed on main in bc4ffcf).
#[test]
fn no_wait_in_either_layer_reports_zero_waited_secs_on_a_ticking_clock() {
    let data = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let gradlew = work.path().join("gradlew");
    fs::write(&gradlew, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&gradlew, fs::Permissions::from_mode(0o755)).unwrap();
    let mut rig = Rig::calm();
    rig.machine.exclusive_build_tools = false;
    let sample = HeavySample {
        peak_rss_bytes: 1024,
        duration_ms: 1,
    };
    record(data.path(), REPO, "job", sample, &rig.sleeper).unwrap();
    let clock = TickingClock(std::sync::atomic::AtomicI64::new(1_000));
    let command = argv(&["./gradlew", "lint"]);
    let done = rig
        .throttle()
        .run_slot_with(
            data.path(),
            &clock,
            &job(work.path(), &command, Some(60)),
            &Inherited::default(),
        )
        .unwrap()
        .unwrap();
    assert!(done.status.success());
    assert_eq!(rig.sleeper.count(), 0);
    assert_eq!(done.waited_secs, 0);
}
