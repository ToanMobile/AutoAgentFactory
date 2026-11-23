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
        inherit_stdin: false,
    };
    assert!(run(&rig, data.path(), &unmeasured_job).is_none());
}
