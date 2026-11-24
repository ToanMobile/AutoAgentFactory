use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;

use tempfile::TempDir;

use super::super::orphans::{record_path, resume_orphans};
use super::super::slots::{Admission, try_acquire};
use super::super::watchdog::{PriorityTools, Watchdog, heavy_argv};
use super::super::{Pressure, SlotGuard};
use super::support::Rig;
use crate::gate::proc::Supervise;

const PGID: u32 = 4242;
const STEP: Duration = Duration::from_millis(20);

fn slot(data: &TempDir) -> SlotGuard {
    let measured = Admission::Measured { peak_rss_bytes: 1 };
    try_acquire(data.path(), 2, measured, &|| Some(u64::MAX))
        .unwrap()
        .unwrap()
}

fn dog<'a>(rig: &'a Rig, data: &'a TempDir, slot: SlotGuard) -> Watchdog<'a> {
    let throttle = rig.throttle();
    Watchdog::new(
        throttle.probe,
        throttle.control,
        throttle.progress,
        data.path(),
        "test_full",
        slot,
        STEP,
    )
}

fn pressures(rig: &Rig, script: &[Pressure]) {
    rig.probe
        .pressure
        .set(script.iter().copied().map(Some).collect());
}

/// Ticks once per sample period.
fn ticks(dog: &mut Watchdog<'_>, count: u32) -> Vec<bool> {
    (0..count).map(|n| dog.tick(PGID, STEP * n)).collect()
}

#[test]
fn slot_zero_is_never_stopped_and_its_peak_is_tracked() {
    let data = TempDir::new().unwrap();
    let rig = Rig::calm();
    pressures(&rig, &[Pressure::Critical]);
    *rig.group.rss.lock().unwrap() = vec![Some(100), Some(900), None, Some(400)].into();
    let mut dog = dog(&rig, &data, slot(&data));
    assert_eq!(dog.slot.index(), 0);

    assert_eq!(ticks(&mut dog, 4), vec![false; 4]);
    assert_eq!(rig.group.stops.load(Ordering::SeqCst), 0);
    assert_eq!(dog.peak_rss_bytes, Some(900));
    assert!(rig.events().is_empty());
}

#[test]
fn later_slot_is_stopped_at_critical_and_continued_at_normal() {
    let data = TempDir::new().unwrap();
    let rig = Rig::calm();
    let _first = slot(&data);
    let second = slot(&data);
    assert_eq!(second.index(), 1);
    pressures(
        &rig,
        &[
            Pressure::Normal,
            Pressure::Warn,
            Pressure::Critical,
            Pressure::Critical,
            // Back to warn is not back to normal: stay stopped.
            Pressure::Warn,
            Pressure::Normal,
            Pressure::Normal,
        ],
    );
    let mut dog = dog(&rig, &data, second);
    assert_eq!(
        ticks(&mut dog, 7),
        vec![false, false, true, true, true, false, false]
    );
    assert_eq!(rig.group.stops.load(Ordering::SeqCst), 1);
    assert_eq!(rig.group.resumes.load(Ordering::SeqCst), 1);
    assert_eq!(
        rig.events(),
        vec![
            "Paused { label: \"test_full\", slot: 1 }",
            "Resumed { label: \"test_full\", slot: 1 }"
        ]
    );
}

#[test]
fn warn_alone_never_stops_a_command() {
    let data = TempDir::new().unwrap();
    let rig = Rig::calm();
    let _first = slot(&data);
    pressures(&rig, &[Pressure::Warn]);
    let mut dog = dog(&rig, &data, slot(&data));
    assert_eq!(ticks(&mut dog, 5), vec![false; 5]);
    assert_eq!(rig.group.stops.load(Ordering::SeqCst), 0);
}

#[test]
fn unreadable_pressure_does_not_stop_and_does_continue() {
    let data = TempDir::new().unwrap();
    let rig = Rig::calm();
    let _first = slot(&data);
    rig.probe
        .pressure
        .set(vec![None, Some(Pressure::Critical), None]);
    let mut dog = dog(&rig, &data, slot(&data));
    assert_eq!(ticks(&mut dog, 3), vec![false, true, false]);
}

#[test]
fn stopped_command_takes_over_slot_zero_when_it_frees() {
    let data = TempDir::new().unwrap();
    let rig = Rig::calm();
    let first = slot(&data);
    pressures(&rig, &[Pressure::Critical]);
    let mut dog = dog(&rig, &data, slot(&data));
    assert_eq!(ticks(&mut dog, 2), vec![true, true]);

    // The slot 0 holder finishes while pressure is still critical: someone
    // must run, so the stopped command is promoted and continued.
    drop(first);
    assert!(!dog.tick(PGID, STEP * 2));
    assert_eq!(dog.slot.index(), 0);
    assert_eq!(rig.group.resumes.load(Ordering::SeqCst), 1);
    // And as the slot 0 holder it is never stopped again.
    assert!(!dog.tick(PGID, STEP * 3));
    assert_eq!(rig.group.stops.load(Ordering::SeqCst), 1);
    // Its old slot is free for the next command.
    assert_eq!(slot(&data).index(), 1);
}

#[test]
fn free_slot_zero_at_critical_promotes_instead_of_stopping() {
    let data = TempDir::new().unwrap();
    let rig = Rig::calm();
    let first = slot(&data);
    let second = slot(&data);
    drop(first);
    pressures(&rig, &[Pressure::Critical]);
    let mut dog = dog(&rig, &data, second);
    assert_eq!(ticks(&mut dog, 2), vec![false, false]);
    assert_eq!(dog.slot.index(), 0);
    assert_eq!(rig.group.stops.load(Ordering::SeqCst), 0);
}

#[test]
fn measurements_are_taken_once_per_sample_period() {
    let data = TempDir::new().unwrap();
    let rig = Rig::calm();
    let _first = slot(&data);
    pressures(&rig, &[Pressure::Critical]);
    let mut dog = dog(&rig, &data, slot(&data));
    // Many polls inside one period: one reading, and the answer is kept.
    assert!(dog.tick(PGID, Duration::ZERO));
    for millis in 1..20 {
        assert!(dog.tick(PGID, Duration::from_millis(millis)));
    }
    assert_eq!(rig.probe.pressure.calls(), 1);
    assert!(dog.tick(PGID, STEP));
    assert_eq!(rig.probe.pressure.calls(), 2);
}

#[test]
fn failed_signals_are_not_taken_for_done() {
    let data = TempDir::new().unwrap();
    let rig = Rig::calm();
    let _first = slot(&data);
    pressures(
        &rig,
        &[Pressure::Critical, Pressure::Critical, Pressure::Normal],
    );
    let mut dog = dog(&rig, &data, slot(&data));

    // SIGSTOP could not be sent: the command is running and is charged.
    *rig.group.refuse.lock().unwrap() = true;
    assert!(!dog.tick(PGID, Duration::ZERO));
    *rig.group.refuse.lock().unwrap() = false;
    assert!(dog.tick(PGID, STEP));

    // SIGCONT could not be sent: still stopped, tried again next time.
    *rig.group.refuse.lock().unwrap() = true;
    assert!(dog.tick(PGID, STEP * 2));
    *rig.group.refuse.lock().unwrap() = false;
    assert!(!dog.tick(PGID, STEP * 3));
    assert_eq!(rig.group.resumes.load(Ordering::SeqCst), 2);
}

#[test]
fn helpers_are_continued_even_when_the_group_resume_fails() {
    use super::super::probe::ProcId;
    use super::super::watchdog::TreeSample;

    let data = TempDir::new().unwrap();
    let rig = Rig::calm();
    let _first = slot(&data);
    let helper = ProcId {
        pid: 7,
        started: "Tue Nov 24 12:00:01 2026".to_string(),
    };
    *rig.group.tree.lock().unwrap() = Some(TreeSample {
        rss_bytes: 4096,
        members: vec![helper.clone()],
    });
    pressures(
        &rig,
        &[Pressure::Critical, Pressure::Normal, Pressure::Normal],
    );
    let mut dog = dog(&rig, &data, slot(&data));
    assert!(dog.tick(PGID, Duration::ZERO));

    // The group's SIGCONT fails (e.g. ESRCH): still paused, no Resumed,
    // but the helper stopped by pid is continued all the same.
    *rig.group.refuse.lock().unwrap() = true;
    assert!(dog.tick(PGID, STEP));
    let sent: Vec<_> = rig.group.signalled.lock().unwrap().clone();
    let cont = ("-CONT".to_string(), vec![helper.clone()]);
    assert_eq!(sent, vec![("-STOP".to_string(), vec![helper]), cont]);
    assert_eq!(
        rig.events(),
        vec!["Paused { label: \"test_full\", slot: 1 }"]
    );

    *rig.group.refuse.lock().unwrap() = false;
    assert!(!dog.tick(PGID, STEP * 2));
    assert_eq!(rig.events().len(), 2);
}

#[test]
fn finish_continues_a_group_that_is_still_stopped() {
    let data = TempDir::new().unwrap();
    let rig = Rig::calm();
    let _first = slot(&data);
    pressures(&rig, &[Pressure::Critical]);
    let mut dog = dog(&rig, &data, slot(&data));
    assert!(dog.tick(PGID, Duration::ZERO));
    dog.finish();
    assert_eq!(rig.group.resumes.load(Ordering::SeqCst), 1);
    // Nothing to do when it was not stopped.
    dog.finish();
    assert_eq!(rig.group.resumes.load(Ordering::SeqCst), 1);
}

#[test]
fn a_stop_leaves_a_record_until_the_group_is_continued() {
    let data = TempDir::new().unwrap();
    let rig = Rig::calm();
    let _first = slot(&data);
    pressures(
        &rig,
        &[Pressure::Critical, Pressure::Critical, Pressure::Normal],
    );
    let record = record_path(data.path(), PGID);
    let mut dog = dog(&rig, &data, slot(&data));
    assert_eq!(ticks(&mut dog, 2), vec![true, true]);
    assert!(record.exists());
    // Its owner is alive, so a sweep by another run leaves it alone.
    assert_eq!(resume_orphans(data.path(), &rig.group), 0);
    assert_eq!(rig.group.resumes.load(Ordering::SeqCst), 0);
    assert!(!dog.tick(PGID, STEP * 2));
    assert!(!record.exists());

    // A stop that could not be sent leaves nothing to recover.
    *rig.group.refuse.lock().unwrap() = true;
    pressures(&rig, &[Pressure::Critical]);
    assert!(!dog.tick(PGID, STEP * 3));
    assert!(!record.exists());
}

fn argv(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

#[test]
fn priority_tools_are_used_only_where_they_exist() {
    let all = |_: &Path| true;
    let none = |_: &Path| false;
    let only_bin_nice = |path: &Path| path == Path::new("/bin/nice");

    let mac = PriorityTools::locate(&all, true);
    assert_eq!(mac.nice, Some(PathBuf::from("/usr/bin/nice")));
    assert_eq!(mac.taskpolicy, Some(PathBuf::from("/usr/sbin/taskpolicy")));
    // taskpolicy is a macOS tool; a same-named file elsewhere is not used.
    assert_eq!(PriorityTools::locate(&all, false).taskpolicy, None);
    assert_eq!(PriorityTools::locate(&none, true), PriorityTools::default());
    let linux = PriorityTools::locate(&only_bin_nice, false);
    assert_eq!(linux.nice, Some(PathBuf::from("/bin/nice")));
}

#[test]
fn heavy_argv_table() {
    let mac = PriorityTools::locate(&|_| true, true);
    let linux = PriorityTools::locate(&|_| true, false);
    let bare = PriorityTools::default();
    let cmd = "make test";
    assert_eq!(
        heavy_argv(cmd, true, &mac),
        argv(&[
            "/usr/sbin/taskpolicy",
            "-c",
            "utility",
            "/usr/bin/nice",
            "-n",
            "10",
            "/bin/sh",
            "-c",
            cmd
        ])
    );
    // `/bin/sh` by its path: the wrappers `exec` their program through
    // `PATH`, where a `sh` the task committed could come first.
    assert_eq!(
        heavy_argv(cmd, true, &linux),
        argv(&["/usr/bin/nice", "-n", "10", "/bin/sh", "-c", cmd])
    );
    // Tools absent, or low priority switched off: a plain run.
    assert_eq!(heavy_argv(cmd, true, &bare), argv(&["/bin/sh", "-c", cmd]));
    assert_eq!(heavy_argv(cmd, false, &mac), argv(&["/bin/sh", "-c", cmd]));
}

#[test]
fn first_tick_reuses_tree_leader_start_without_extra_group_state_scan() {
    use std::fs;
    use std::sync::atomic::AtomicUsize;

    use super::super::GroupControl;
    use super::super::probe::{GroupState, ProcId};
    use super::super::slots::slot_path;
    use super::super::watchdog::TreeSample;

    struct CountingGroup {
        tree_sample: Option<TreeSample>,
        fallback_started: Option<String>,
        tree_calls: AtomicUsize,
        group_state_calls: AtomicUsize,
    }

    impl GroupControl for CountingGroup {
        fn rss_bytes(&self, _pgid: u32) -> Option<u64> {
            Some(4096)
        }
        fn stop(&self, _pgid: u32) -> bool {
            true
        }
        fn resume(&self, _pgid: u32) -> bool {
            true
        }
        fn group_state(&self, _pgid: u32) -> Option<GroupState> {
            self.group_state_calls.fetch_add(1, Ordering::SeqCst);
            Some(GroupState {
                leader_started: self.fallback_started.clone(),
                stopped: false,
            })
        }
        fn tree(&self, _pgid: u32, _known: &[ProcId]) -> Option<TreeSample> {
            self.tree_calls.fetch_add(1, Ordering::SeqCst);
            self.tree_sample.clone()
        }
    }

    // Case 1: tree() returns leader info -> no redundant group_state() call on tick 0.
    let data = TempDir::new().unwrap();
    let rig = Rig::calm();
    let throttle = rig.throttle();
    let control = CountingGroup {
        tree_sample: Some(TreeSample {
            rss_bytes: 8192,
            members: vec![ProcId {
                pid: PGID,
                started: "Mon Nov 23 14:00:00 2026".to_string(),
            }],
        }),
        fallback_started: Some("SHOULD_NOT_BE_USED".to_string()),
        tree_calls: AtomicUsize::new(0),
        group_state_calls: AtomicUsize::new(0),
    };
    let mut dog = Watchdog::new(
        throttle.probe,
        &control,
        throttle.progress,
        data.path(),
        "test_full",
        slot(&data),
        STEP,
    );
    assert!(!dog.tick(PGID, Duration::ZERO));
    assert_eq!(control.tree_calls.load(Ordering::SeqCst), 1);
    assert_eq!(control.group_state_calls.load(Ordering::SeqCst), 0);
    assert_eq!(dog.peak_rss_bytes, Some(8192));
    let text = fs::read_to_string(slot_path(data.path(), 0)).unwrap();
    assert!(text.contains("lstart=Mon Nov 23 14:00:00 2026"), "{text:?}");
    drop(dog);

    // Case 2: tree() returns None -> falls back to group_state() on tick 0.
    let data2 = TempDir::new().unwrap();
    let control2 = CountingGroup {
        tree_sample: None,
        fallback_started: Some("Mon Nov 23 15:00:00 2026".to_string()),
        tree_calls: AtomicUsize::new(0),
        group_state_calls: AtomicUsize::new(0),
    };
    let mut dog2 = Watchdog::new(
        throttle.probe,
        &control2,
        throttle.progress,
        data2.path(),
        "test_full",
        slot(&data2),
        STEP,
    );
    assert!(!dog2.tick(PGID, Duration::ZERO));
    assert_eq!(control2.tree_calls.load(Ordering::SeqCst), 1);
    assert_eq!(control2.group_state_calls.load(Ordering::SeqCst), 1);
    assert_eq!(dog2.peak_rss_bytes, Some(4096));
    let text2 = fs::read_to_string(slot_path(data2.path(), 0)).unwrap();
    assert!(
        text2.contains("lstart=Mon Nov 23 15:00:00 2026"),
        "{text2:?}"
    );
}
