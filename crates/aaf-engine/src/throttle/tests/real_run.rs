//! Real runs of small commands through the whole throttle: priority,
//! stop/continue, timeout and memory sampling as the OS really does them.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tempfile::TempDir;

use super::super::probe::{GroupState, Unreadable};
use super::super::slots::{Admission, try_acquire};
use super::super::stats::{HeavySample, record, recorded_peak, samples};
use super::super::{
    GroupControl, HeavyJob, LoadProbe, Pressure, PriorityTools, SlotGuard, SystemGroup, Throttle,
};
use super::support::{GIB, MarksSampled, Rig, after_file};

const REPO: &str = "repo";
const LONG: Duration = Duration::from_secs(120);

fn job<'a>(cwd: &'a Path, command: &'a str, timeout: Duration) -> HeavyJob<'a> {
    HeavyJob {
        repo_id: REPO,
        label: "test_full",
        command,
        cwd,
        timeout,
        log_tail_bytes: 4096,
    }
}

fn ps(field: &str, pid: &str) -> String {
    let out = Command::new("ps")
        .args(["-o", field, "-p", pid])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn occupy(data: &TempDir) -> SlotGuard {
    let measured = Admission::Measured { peak_rss_bytes: 1 };
    try_acquire(data.path(), 1, measured, &|| None)
        .unwrap()
        .unwrap()
}

#[test]
fn heavy_command_really_runs_niced_and_plain_when_switched_off() {
    let data = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let mut rig = Rig::calm();
    rig.tools = PriorityTools::detect();
    if cfg!(target_os = "macos") {
        assert!(rig.tools.nice.is_some(), "{:?}", rig.tools);
        assert!(rig.tools.taskpolicy.is_some(), "{:?}", rig.tools);
    }
    let own: i32 = ps("nice=", &std::process::id().to_string())
        .parse()
        .unwrap();
    let nice_of_child = |rig: &Rig| -> i32 {
        let job = job(work.path(), "ps -o nice= -p $$", LONG);
        let run = rig
            .throttle()
            .run_heavy(data.path(), &*rig.clock, &job)
            .unwrap();
        assert_eq!(run.exit_code, Some(0), "{:?}", run);
        String::from_utf8_lossy(&run.output).trim().parse().unwrap()
    };

    let niced = if rig.tools.nice.is_some() {
        (own + 10).min(20)
    } else {
        own
    };
    assert_eq!(nice_of_child(&rig), niced);
    rig.machine.low_priority = false;
    assert_eq!(nice_of_child(&rig), own);
}

#[test]
fn command_runs_in_its_directory_and_keeps_the_output_tail() {
    let data = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    fs::write(work.path().join("marker.txt"), "x").unwrap();
    let rig = Rig::calm();
    let job = job(work.path(), "ls; echo oops >&2; exit 3", LONG);
    let run = rig
        .throttle()
        .run_heavy(data.path(), &*rig.clock, &job)
        .unwrap();
    assert_eq!(run.exit_code, Some(3));
    assert!(!run.timed_out);
    let output = String::from_utf8_lossy(&run.output);
    assert!(output.contains("marker.txt") && output.contains("oops"));
    assert_eq!((run.slot, run.waited_secs, run.degraded), (0, 0, false));
}

#[test]
fn timeout_still_kills_a_heavy_command_and_nothing_is_learned_from_it() {
    let data = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let mut rig = Rig::calm();
    rig.real_group = true;
    let job = job(work.path(), "sleep 300", Duration::from_millis(400));
    let started = Instant::now();
    let run = rig
        .throttle()
        .run_heavy(data.path(), &*rig.clock, &job)
        .unwrap();
    assert!(run.timed_out);
    assert!(started.elapsed() < Duration::from_secs(60));
    assert_eq!(recorded_peak(data.path(), REPO, "test_full"), None);
}

#[test]
fn passing_run_records_its_real_peak_memory_and_a_failing_one_does_not() {
    let data = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let rig = Rig::calm();
    // Each command waits until it has been sampled: under load the first
    // sample came after a `sleep 0.5` had ended and read 0 bytes.
    let marked = |name: &str| MarksSampled {
        file: work.path().join(name),
    };

    let control = marked("failing.sampled");
    let failing = after_file(&control.file, "sleep 0.5; exit 3");
    let failing = job(work.path(), &failing, LONG);
    let run = Throttle {
        control: &control,
        ..rig.throttle()
    }
    .run_heavy(data.path(), &*rig.clock, &failing)
    .unwrap();
    assert_eq!(run.exit_code, Some(3));
    // It was sampled, but a failed run says little about a full one.
    assert!(run.peak_rss_bytes.unwrap() > 0);
    assert_eq!(recorded_peak(data.path(), REPO, "test_full"), None);

    let control = marked("passing.sampled");
    let passing = after_file(&control.file, "sleep 0.5");
    let passing = job(work.path(), &passing, LONG);
    let run = Throttle {
        control: &control,
        ..rig.throttle()
    }
    .run_heavy(data.path(), &*rig.clock, &passing)
    .unwrap();
    assert_eq!(run.exit_code, Some(0));
    let peak = run.peak_rss_bytes.unwrap();
    // `sh` and `sleep` are small but not nothing, and nowhere near a GiB.
    assert!(peak > 64 * 1024 && peak < GIB, "{peak}");
    assert_eq!(recorded_peak(data.path(), REPO, "test_full"), Some(peak));
    let kept = samples(data.path(), REPO, "test_full");
    assert_eq!(kept.len(), 1);
    assert!(kept[0].duration_ms >= 400, "{kept:?}");
}

/// Command timeout of the pausing run: above the longest the running part
/// can take however slow the machine, so that only a charged pause can use
/// it up. With [`Unsampled`] the watchdog runs two `ps` before the stop
/// (the leader's start time, for the slot record and for the pause record),
/// each cut off at `PROBE_TIMEOUT` (2 s), and none after the resume; the
/// rest is a few process starts. It was 3 s, and a run
/// whose samples each took a `ps` was charged 7.3 s of running time under
/// load 27-77 and killed.
const PAUSE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the group is kept stopped once it has been seen stopped.
/// Longer than [`PAUSE_TIMEOUT`]: if paused time were charged, the command
/// would be killed right after it is continued. The second of margin covers
/// the time a sample takes before it asks for the pressure, which is
/// counted as paused only up to the resume.
const PAUSE_HOLD: Duration = Duration::from_secs(11);

/// The real group without its memory samples: stop, continue and the
/// group's state are what the pausing run is about. A sample is a `ps` run
/// per poll, up to two seconds each on a loaded machine, all of it charged
/// to the command's running time.
struct Unsampled;

impl GroupControl for Unsampled {
    fn rss_bytes(&self, _pgid: u32) -> Option<u64> {
        None
    }

    fn stop(&self, pgid: u32) -> bool {
        SystemGroup.stop(pgid)
    }

    fn resume(&self, pgid: u32) -> bool {
        SystemGroup.resume(pgid)
    }

    fn group_state(&self, pgid: u32) -> Option<GroupState> {
        SystemGroup.group_state(pgid)
    }

    fn read_group_state(&self, pgid: u32) -> Result<Option<GroupState>, Unreadable> {
        SystemGroup.read_group_state(pgid)
    }
}

#[derive(Default)]
struct PauseState {
    /// The first critical answer has been given.
    asked: bool,
    /// When `ps` first showed the command stopped.
    stopped_at: Option<Instant>,
    released: bool,
}

/// Critical pressure from the first look after the command has written its
/// pid until the group has been seen stopped for [`PAUSE_HOLD`]; normal from
/// then on, when it also writes `release` so the command can end. While
/// critical it notes the process state `ps` reports.
///
/// Measured from the stop the probe saw, not from the first answer, so the
/// whole hold is spent stopped however long the stop took on a loaded
/// machine.
struct PauseProbe {
    pid_file: PathBuf,
    release: PathBuf,
    state: Mutex<PauseState>,
    states: Mutex<Vec<String>>,
}

impl PauseProbe {
    fn new(pid_file: PathBuf, release: PathBuf) -> PauseProbe {
        PauseProbe {
            pid_file,
            release,
            state: Mutex::new(PauseState::default()),
            states: Mutex::new(Vec::new()),
        }
    }
}

impl LoadProbe for PauseProbe {
    fn load_percent(&self) -> Option<u32> {
        Some(0)
    }

    fn pressure(&self) -> Option<Pressure> {
        let pid = fs::read_to_string(&self.pid_file).unwrap_or_default();
        let pid = pid.trim();
        if pid.is_empty() {
            return Some(Pressure::Normal);
        }
        let mut state = self.state.lock().unwrap();
        if state.released {
            return Some(Pressure::Normal);
        }
        if !state.asked {
            state.asked = true;
            return Some(Pressure::Critical);
        }
        // The previous answer was critical, so by now the group must
        // already be stopped.
        let seen = ps("state=", pid);
        if seen.starts_with('T') && state.stopped_at.is_none() {
            state.stopped_at = Some(Instant::now());
        }
        self.states.lock().unwrap().push(seen);
        if state
            .stopped_at
            .is_some_and(|stopped| stopped.elapsed() >= PAUSE_HOLD)
        {
            state.released = true;
            fs::write(&self.release, "").unwrap();
            return Some(Pressure::Normal);
        }
        Some(Pressure::Critical)
    }

    fn free_memory_bytes(&self) -> Option<u64> {
        Some(u64::MAX)
    }
}

fn pausing_run(holder_of_slot_zero: bool) -> (super::super::HeavyRun, Vec<String>, Vec<String>) {
    let data = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let mut rig = Rig::calm();
    rig.real_group = true;
    rig.machine.low_priority = false;
    rig.sample_every = Duration::from_millis(50);
    let sample = HeavySample {
        peak_rss_bytes: 1024,
        duration_ms: 1,
    };
    record(data.path(), REPO, "test_full", sample, &rig.sleeper).unwrap();
    let _first = holder_of_slot_zero.then(|| occupy(&data));

    let pid_file = work.path().join("pid");
    let release = work.path().join("release");
    let probe = PauseProbe::new(pid_file.clone(), release.clone());
    let throttle = Throttle {
        probe: &probe,
        control: &Unsampled,
        ..rig.throttle()
    };
    let write_pid = format!("echo $$ > '{}'", pid_file.display());
    // A later slot's command runs until the probe lets the pressure ease:
    // a fixed `sleep 0.3` could end, on a loaded machine, before the
    // watchdog first asked for the pressure, and nothing was stopped. The
    // holder of slot 0 is never asked, so its command just ends.
    let script = if holder_of_slot_zero {
        format!("{write_pid}; {}", after_file(&release, "exit 0"))
    } else {
        format!("{write_pid}; exec sleep 0.3")
    };
    let job = job(work.path(), &script, PAUSE_TIMEOUT);
    let run = throttle.run_heavy(data.path(), &*rig.clock, &job).unwrap();
    let states = probe.states.lock().unwrap().clone();
    (run, states, rig.events())
}

#[test]
fn later_slot_is_really_stopped_and_the_pause_is_not_charged_to_the_timeout() {
    let (run, states, events) = pausing_run(true);
    assert_eq!(run.slot, 1);
    assert!(!states.is_empty());
    assert!(
        states.iter().all(|state| state.starts_with('T')),
        "{states:?}"
    );
    assert!(!run.timed_out, "{run:?}");
    assert_eq!(run.exit_code, Some(0));
    // Stopped for longer than the whole timeout, and not killed for it.
    assert!(run.paused > PAUSE_TIMEOUT, "{:?}", run.paused);
    // The pause is not in the running time.
    assert!(run.duration < PAUSE_TIMEOUT, "{:?}", run.duration);
    assert_eq!(
        events,
        vec![
            "Paused { label: \"test_full\", slot: 1 }",
            "Resumed { label: \"test_full\", slot: 1 }"
        ]
    );
}

#[test]
fn slot_zero_is_really_never_stopped() {
    let (run, states, events) = pausing_run(false);
    assert_eq!(run.slot, 0);
    // The slot 0 holder does not even ask about pressure.
    assert!(states.is_empty(), "{states:?}");
    assert_eq!(run.exit_code, Some(0));
    assert_eq!(run.paused, Duration::ZERO);
    assert!(events.is_empty(), "{events:?}");
}
