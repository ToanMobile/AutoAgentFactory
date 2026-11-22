use std::cell::Cell;

use tempfile::TempDir;

use super::super::slots::{Admission, Busy, ceiling, required_free, slot_path, try_acquire};
use super::support::GIB;

const MEASURED: Admission = Admission::Measured {
    peak_rss_bytes: 4 * GIB,
};

fn free(bytes: Option<u64>) -> impl Fn() -> Option<u64> {
    move || bytes
}

#[test]
fn ceiling_is_explicit_or_a_quarter_of_the_cpus() {
    assert_eq!(ceiling(0, 12), 3);
    assert_eq!(ceiling(0, 8), 2);
    assert_eq!(ceiling(0, 3), 1);
    assert_eq!(ceiling(0, 0), 1);
    assert_eq!(ceiling(1, 64), 1);
    assert_eq!(ceiling(5, 2), 5);
}

#[test]
fn slot_files_live_under_throttle_in_the_data_dir() {
    let data = TempDir::new().unwrap();
    let guard = try_acquire(data.path(), 2, MEASURED, &free(None))
        .unwrap()
        .unwrap();
    assert_eq!(guard.index(), 0);
    assert_eq!(
        slot_path(data.path(), 0),
        data.path().join("throttle/heavy.0.lock")
    );
    assert!(slot_path(data.path(), 0).exists());
}

#[test]
fn slot_zero_admits_without_asking_for_memory() {
    let data = TempDir::new().unwrap();
    let asked = Cell::new(false);
    let probe = || {
        asked.set(true);
        Some(0)
    };
    // Free memory is zero and it still gets the first slot.
    let guard = try_acquire(data.path(), 3, MEASURED, &probe)
        .unwrap()
        .unwrap();
    assert_eq!(guard.index(), 0);
    assert!(!asked.get());
}

#[test]
fn later_slot_needs_free_memory_for_the_recorded_peak() {
    let data = TempDir::new().unwrap();
    let _first = try_acquire(data.path(), 3, MEASURED, &free(None))
        .unwrap()
        .unwrap();
    let needed = required_free(4 * GIB);
    assert_eq!(needed, 5 * GIB);

    // What each command already holding a slot is still expected to take
    // (none of them has started) is set aside before the newcomer's need.
    let held = needed;

    // Made to wait: one byte short.
    let short = try_acquire(data.path(), 3, MEASURED, &free(Some(held + needed - 1))).unwrap();
    assert_eq!(
        short.unwrap_err(),
        Busy::NotEnoughMemory {
            free: Some(needed - 1),
            needed
        }
    );
    // Made to wait: free memory unknown.
    let unknown = try_acquire(data.path(), 3, MEASURED, &free(None)).unwrap();
    assert_eq!(
        unknown.unwrap_err(),
        Busy::NotEnoughMemory { free: None, needed }
    );
    // Admitted: exactly 1.25 x the peak.
    let second = try_acquire(data.path(), 3, MEASURED, &free(Some(held + needed)))
        .unwrap()
        .unwrap();
    assert_eq!(second.index(), 1);
    let third = try_acquire(data.path(), 3, MEASURED, &free(Some(2 * held + needed)))
        .unwrap()
        .unwrap();
    assert_eq!(third.index(), 2);
    // The ceiling holds however much memory is free.
    let fourth = try_acquire(data.path(), 3, MEASURED, &free(Some(u64::MAX))).unwrap();
    assert_eq!(fourth.unwrap_err(), Busy::SlotsTaken);
}

#[test]
fn ceiling_of_one_never_runs_two() {
    let data = TempDir::new().unwrap();
    let _first = try_acquire(data.path(), 1, MEASURED, &free(None))
        .unwrap()
        .unwrap();
    let second = try_acquire(data.path(), 1, MEASURED, &free(Some(u64::MAX))).unwrap();
    assert_eq!(second.unwrap_err(), Busy::SlotsTaken);
}

#[test]
fn dropping_the_guard_frees_the_slot() {
    let data = TempDir::new().unwrap();
    let first = try_acquire(data.path(), 1, MEASURED, &free(None))
        .unwrap()
        .unwrap();
    assert!(
        try_acquire(data.path(), 1, MEASURED, &free(None))
            .unwrap()
            .is_err()
    );
    drop(first);
    let again = try_acquire(data.path(), 1, MEASURED, &free(None))
        .unwrap()
        .unwrap();
    assert_eq!(again.index(), 0);
}

#[test]
fn unmeasured_command_runs_alone() {
    let data = TempDir::new().unwrap();
    let plenty = free(Some(u64::MAX));

    // Admitted on an idle machine, and nothing may start beside it.
    let alone = try_acquire(data.path(), 3, Admission::Unmeasured, &plenty)
        .unwrap()
        .unwrap();
    assert_eq!(alone.index(), 0);
    let beside = try_acquire(data.path(), 3, MEASURED, &plenty).unwrap();
    assert_eq!(beside.unwrap_err(), Busy::SlotsTaken);
    drop(alone);

    // Made to wait while slot 0 is busy, however much memory is free.
    let first = try_acquire(data.path(), 3, MEASURED, &plenty)
        .unwrap()
        .unwrap();
    let waits = try_acquire(data.path(), 3, Admission::Unmeasured, &plenty).unwrap();
    assert_eq!(waits.unwrap_err(), Busy::RunsAlone);

    // Made to wait while only a later slot is busy, and slot 0 is handed
    // back so the next measured command can still use it.
    let second = try_acquire(data.path(), 3, MEASURED, &plenty)
        .unwrap()
        .unwrap();
    assert_eq!(second.index(), 1);
    drop(first);
    let waits = try_acquire(data.path(), 3, Admission::Unmeasured, &plenty).unwrap();
    assert_eq!(waits.unwrap_err(), Busy::RunsAlone);
    let third = try_acquire(data.path(), 3, MEASURED, &plenty)
        .unwrap()
        .unwrap();
    assert_eq!(third.index(), 0);
}

#[test]
fn promote_moves_a_later_slot_to_slot_zero_when_it_is_free() {
    let data = TempDir::new().unwrap();
    let plenty = free(Some(u64::MAX));
    let first = try_acquire(data.path(), 2, MEASURED, &plenty)
        .unwrap()
        .unwrap();
    let mut second = try_acquire(data.path(), 2, MEASURED, &plenty)
        .unwrap()
        .unwrap();
    assert_eq!(second.index(), 1);

    // Not promoted while slot 0 is held.
    assert!(!second.promote(data.path(), &|_| None).unwrap());
    assert_eq!(second.index(), 1);

    drop(first);
    assert!(second.promote(data.path(), &|_| None).unwrap());
    assert_eq!(second.index(), 0);
    // Slot 1 was given up, slot 0 is now the held one.
    let other = try_acquire(data.path(), 2, MEASURED, &plenty)
        .unwrap()
        .unwrap();
    assert_eq!(other.index(), 1);
    let none = try_acquire(data.path(), 2, MEASURED, &plenty).unwrap();
    assert_eq!(none.unwrap_err(), Busy::SlotsTaken);
}
