//! A data dir from before the throttle had its own directory: an `aaf` of
//! that version may still hold `locks/heavy.<n>.lock` or `locks/drain.lock`
//! while this version starts.

use std::fs::{self, File};
use std::path::Path;

use tempfile::TempDir;

use super::super::probe::GroupState;
use super::super::slots::{
    Admission, Busy, MachineView, holds_token_in, is_held, slot_path, try_acquire, try_acquire_in,
};
use super::super::token::SlotToken;

const SMALL: Admission = Admission::Measured { peak_rss_bytes: 1 };
const TOKEN: &str = "0123456789abcdef0123456789abcdef";
const STARTED: &str = "Sun Nov 22 11:00:00 2026";

fn plenty() -> Option<u64> {
    Some(u64::MAX)
}

/// The lock file of the old version, with `content`, as its holder has it.
fn old_file(data: &Path, name: &str, content: &str) -> File {
    fs::create_dir_all(data.join("locks")).unwrap();
    let path = data.join("locks").join(name);
    fs::write(&path, content).unwrap();
    File::options().write(true).open(path).unwrap()
}

#[test]
fn a_slot_held_through_its_old_lock_file_is_taken() {
    let data = TempDir::new().unwrap();
    let record = format!("token={TOKEN}\nreserved=unknown\n");
    let old = old_file(data.path(), "heavy.0.lock", &record);
    old.lock().unwrap();

    assert!(is_held(data.path(), 0));
    let alone = try_acquire(data.path(), 1, Admission::Unmeasured, &plenty).unwrap();
    assert_eq!(alone.unwrap_err(), Busy::RunsAlone);
    let shared = try_acquire(data.path(), 1, SMALL, &plenty).unwrap();
    assert_eq!(shared.unwrap_err(), Busy::SlotsTaken);
    // Beside a holder of unknown size nothing is admitted to another slot.
    let beside = try_acquire(data.path(), 2, SMALL, &plenty).unwrap();
    assert_eq!(beside.unwrap_err(), Busy::SlotsTaken);
    // The refusals left nothing locked in the new place, and the old
    // holder's record as it was.
    // Unlocked by hand, not by the close: a child another test thread is
    // spawning at this moment holds a copy of the descriptor until its
    // exec, and the lock with it (`accounts/lease.rs`); `is_held` below
    // then saw this probe's own lock.
    let probe = File::open(slot_path(data.path(), 0)).unwrap();
    assert!(probe.try_lock().is_ok());
    probe.unlock().unwrap();
    assert_eq!(
        fs::read_to_string(data.path().join("locks/heavy.0.lock")).unwrap(),
        record
    );
    // A process below the old holder's command is inside that slot.
    let token = SlotToken::parse(TOKEN).unwrap();
    assert!(holds_token_in(data.path(), 0, &token, &|_| None));

    old.unlock().unwrap();
    assert!(!is_held(data.path(), 0));
    let admitted = try_acquire(data.path(), 1, Admission::Unmeasured, &plenty).unwrap();
    assert_eq!(admitted.unwrap().index(), 0);
}

#[test]
fn the_command_of_a_killed_old_holder_still_occupies_its_slot() {
    let data = TempDir::new().unwrap();
    // Nobody holds the old lock; its text names a group that still runs.
    let left = format!("token={TOKEN}\nreserved=unknown\npgid=4242\nlstart={STARTED}\n");
    drop(old_file(data.path(), "heavy.0.lock", &left));
    let running = |pgid: u32| {
        (pgid == 4242).then(|| GroupState {
            stopped: false,
            leader_started: Some(STARTED.to_string()),
        })
    };
    let gone = |_: u32| None;
    let view = |group_state| MachineView {
        free_memory: &plenty,
        group_rss: &|_| None,
        group_state,
    };

    let refused = try_acquire_in(data.path(), 1, Admission::Unmeasured, &view(&running)).unwrap();
    assert_eq!(refused.unwrap_err(), Busy::Orphaned { pgid: 4242 });

    // Once the group is gone the slot is free, and what it left is removed.
    let admitted = try_acquire_in(data.path(), 1, Admission::Unmeasured, &view(&gone)).unwrap();
    assert_eq!(admitted.unwrap().index(), 0);
    assert_eq!(
        fs::read_to_string(data.path().join("locks/heavy.0.lock")).unwrap(),
        ""
    );
}

#[test]
fn an_old_drain_lock_closes_the_slots_to_measured_commands() {
    let data = TempDir::new().unwrap();
    let old = old_file(data.path(), "drain.lock", "");
    old.lock().unwrap();

    let refused = try_acquire(data.path(), 2, SMALL, &plenty).unwrap();
    assert_eq!(refused.unwrap_err(), Busy::Draining);

    old.unlock().unwrap();
    let admitted = try_acquire(data.path(), 2, SMALL, &plenty).unwrap();
    assert_eq!(admitted.unwrap().index(), 0);
}
