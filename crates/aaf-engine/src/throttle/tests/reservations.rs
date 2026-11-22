//! Admission counts what the commands already admitted are still going to
//! take: a JVM that started a second ago holds a few hundred MB of the
//! gigabytes it will reach, and free memory alone does not show that.

use std::fs::{self, File};
use std::sync::{Arc, Mutex};

use tempfile::TempDir;

use super::super::slots::{
    Admission, Busy, MachineView, read_reservation, required_free, slot_path, try_acquire,
    try_acquire_in,
};
use super::super::stats::{HeavySample, record};
use super::support::{GIB, Rig};

const MIB: u64 = 1024 * 1024;

fn measured(gib: u64) -> Admission {
    Admission::Measured {
        peak_rss_bytes: gib * GIB,
    }
}

/// A machine with `free` bytes free where group 4242 uses `rss` bytes.
fn attempt(
    data: &TempDir,
    admission: Admission,
    free: u64,
    rss: Option<u64>,
) -> Result<usize, Busy> {
    let view = MachineView {
        free_memory: &|| Some(free),
        group_rss: &|pgid| if pgid == 4242 { rss } else { None },
        group_state: &|_| None,
    };
    try_acquire_in(data.path(), 3, admission, &view)
        .unwrap()
        .map(|guard| guard.index())
}

#[test]
fn second_six_gib_command_waits_while_the_first_has_not_grown_yet() {
    let data = TempDir::new().unwrap();
    let mut first = try_acquire(data.path(), 3, measured(6), &|| Some(8 * GIB))
        .unwrap()
        .unwrap();
    let needed = required_free(6 * GIB);

    // Admitted but not started: all of its 7.5 GiB are still to come.
    assert_eq!(
        attempt(&data, measured(6), 8 * GIB, None),
        Err(Busy::NotEnoughMemory {
            free: Some(GIB / 2),
            needed
        })
    );
    // Started a second ago: it holds 300 MiB of them.
    first.record_pgid(4242);
    assert_eq!(
        attempt(&data, measured(6), 8 * GIB, Some(300 * MIB)),
        Err(Busy::NotEnoughMemory {
            free: Some(GIB / 2 + 300 * MIB),
            needed
        })
    );
    // Fully grown: what is free now is really free.
    assert_eq!(attempt(&data, measured(6), 8 * GIB, Some(9 * GIB)), Ok(1));
}

#[test]
fn two_two_gib_commands_both_run_in_eight_gib_free() {
    let data = TempDir::new().unwrap();
    let _first = try_acquire(data.path(), 3, measured(2), &|| Some(8 * GIB))
        .unwrap()
        .unwrap();
    // 8 GiB free, 2.5 reserved by the first, 2.5 needed.
    assert_eq!(attempt(&data, measured(2), 8 * GIB, None), Ok(1));
}

#[test]
fn every_busy_slot_counts_and_a_refused_attempt_leaves_nothing_behind() {
    let data = TempDir::new().unwrap();
    let plenty = || Some(64 * GIB);
    let _first = try_acquire(data.path(), 3, measured(2), &plenty)
        .unwrap()
        .unwrap();
    let _second = try_acquire(data.path(), 3, measured(2), &plenty)
        .unwrap()
        .unwrap();
    // 2.5 + 2.5 reserved, 2.5 needed: 7.5 GiB is the least that admits.
    assert_eq!(
        attempt(&data, measured(2), 15 * GIB / 2 - 1, None),
        Err(Busy::NotEnoughMemory {
            free: Some(5 * GIB / 2 - 1),
            needed: 5 * GIB / 2
        })
    );
    // The refused attempt held slot 2 for a moment; nothing of it is left.
    assert_eq!(read_reservation(data.path(), 2), None);
    assert_eq!(attempt(&data, measured(2), 15 * GIB / 2, None), Ok(2));
}

#[test]
fn slot_zero_is_not_free_of_the_rule_while_a_later_slot_is_busy() {
    let data = TempDir::new().unwrap();
    let plenty = || Some(64 * GIB);
    let first = try_acquire(data.path(), 3, measured(6), &plenty)
        .unwrap()
        .unwrap();
    let _second = try_acquire(data.path(), 3, measured(6), &plenty)
        .unwrap()
        .unwrap();
    drop(first);
    // Slot 0 is free, but the command in slot 1 has 7.5 GiB still to take.
    assert!(matches!(
        attempt(&data, measured(6), 8 * GIB, None),
        Err(Busy::NotEnoughMemory { .. })
    ));
    assert_eq!(attempt(&data, measured(6), 15 * GIB, None), Ok(0));
}

#[test]
fn reservation_is_written_on_acquire_follows_a_promotion_and_goes_on_release() {
    let data = TempDir::new().unwrap();
    let plenty = || Some(64 * GIB);
    let first = try_acquire(data.path(), 2, measured(4), &plenty)
        .unwrap()
        .unwrap();
    let mut second = try_acquire(data.path(), 2, measured(2), &plenty)
        .unwrap()
        .unwrap();
    second.record_pgid(77);
    let of = |index| read_reservation(data.path(), index).map(|r| (r.reserved_bytes, r.pgid));
    assert_eq!(of(0), Some((Some(5 * GIB), None)));
    assert_eq!(of(1), Some((Some(5 * GIB / 2), Some(77))));

    drop(first);
    assert_eq!(of(0), None);
    assert!(second.promote(data.path(), &|_| None).unwrap());
    assert_eq!(of(0), Some((Some(5 * GIB / 2), Some(77))));
    assert_eq!(of(1), None);

    // A command of unknown size says so in every slot it holds.
    drop(second);
    let _alone = try_acquire(data.path(), 2, Admission::Unmeasured, &plenty)
        .unwrap()
        .unwrap();
    assert_eq!(of(0), Some((None, None)));
    assert_eq!(of(1), Some((None, None)));
}

#[test]
fn busy_slot_with_an_unreadable_reservation_never_admits_and_never_errors() {
    for content in [
        "",
        "garbage",
        "reserved=abc\n",
        "pgid=12\n",
        "reserved=unknown\n",
        "reserved=-5\n",
        "\u{0}\u{1}\u{2}",
    ] {
        let data = TempDir::new().unwrap();
        let path = slot_path(data.path(), 0);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
        // Held the way another process would hold it.
        let held = File::open(&path).unwrap();
        held.lock().unwrap();

        let outcome = attempt(&data, measured(1), u64::MAX, None);
        assert_eq!(outcome, Err(Busy::SlotsTaken), "{content:?}");
        held.unlock().unwrap();
    }
}

#[test]
fn second_command_waits_in_admit_until_the_first_is_done() {
    let data = TempDir::new().unwrap();
    let mut rig = Rig::calm();
    rig.machine.max_heavy_parallel = 2;
    rig.probe.free.set(vec![Some(8 * GIB)]);
    let sample = HeavySample {
        peak_rss_bytes: 6 * GIB,
        duration_ms: 10,
    };
    assert!(record(data.path(), "repo", "test_full", sample, &rig.sleeper).unwrap());

    let throttle = rig.throttle();
    let first = throttle
        .admit(data.path(), &*rig.clock, "repo", "test_full")
        .unwrap();
    assert_eq!(first.slot.index(), 0);
    let first = Arc::new(Mutex::new(Some(first)));
    let release = Arc::clone(&first);
    rig.sleeper.on_sleep(move |count| {
        if count == 3 {
            release.lock().unwrap().take();
        }
    });

    let second = throttle
        .admit(data.path(), &*rig.clock, "repo", "test_full")
        .unwrap();
    assert_eq!(rig.sleeper.count(), 3);
    assert_eq!(second.slot.index(), 0);
    assert!(second.waited_secs >= 3);
    assert!(first.lock().unwrap().is_none());
}
