//! A guest record planted in `guest/` while a holder outside the sandbox is
//! taking a slot. The lock file decides whose record is read: empty means a
//! guest holds the slot. A holder that left it empty after taking the lock
//! (while it looked at the other slots, each look a `ps` run) had the
//! planted record read as its own, token included.

use std::cell::RefCell;
use std::fs;
use std::path::Path;

use tempfile::TempDir;

use super::super::paths::{guest_record_path, token_path};
use super::super::probe::GroupState;
use super::super::slots::{
    Admission, MachineView, Reservation, holds_token, is_held, read_reservation, slot_path,
    try_acquire_in,
};
use super::super::token::SlotToken;

/// What the agent claims: its own token, and a reservation that would let
/// anything run beside it.
const PLANTED: &str = "fedcba9876543210fedcba9876543210";
const KILLED_GROUP: u32 = 4242;

fn plant_guest_record(data: &Path, index: usize) {
    let path = guest_record_path(data, index);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, format!("token={PLANTED}\nreserved=1\npgid=99\n")).unwrap();
}

/// Slot 1 holds what a killed holder left, so that taking the slots looks
/// the group up in the process table after slot 0 is already locked.
fn killed_holder_in_slot_1(data: &Path) {
    let path = slot_path(data, 1);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        format!("reserved=unknown\npgid={KILLED_GROUP}\nlstart=Sun Nov 22 11:00:00 2026\n"),
    )
    .unwrap();
}

/// What a reader of slot 0 saw while the acquisition was looking at slot 1.
#[derive(Debug, Default)]
struct Seen {
    held: bool,
    reservation: Option<Reservation>,
    planted_token_holds: bool,
}

fn look_during_acquisition(admission: Admission) -> Seen {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path();
    killed_holder_in_slot_1(data);
    let planted = SlotToken::parse(PLANTED).unwrap();
    let seen = RefCell::new(None);
    let group_state = |pgid: u32| -> Option<GroupState> {
        if pgid == KILLED_GROUP && seen.borrow().is_none() {
            // The agent writes after whatever the holder removed.
            plant_guest_record(data, 0);
            *seen.borrow_mut() = Some(Seen {
                held: is_held(data, 0),
                reservation: read_reservation(data, 0),
                planted_token_holds: holds_token(data, 0, &planted),
            });
        }
        // The killed holder's command is gone: slot 1 is free.
        None
    };
    let view = MachineView {
        free_memory: &|| Some(u64::MAX),
        group_rss: &|_| None,
        group_state: &group_state,
    };
    let guard = try_acquire_in(data, 2, admission, &view).unwrap().unwrap();
    assert_eq!(guard.index(), 0);
    seen.into_inner().expect("slot 1 was looked at")
}

#[test]
fn a_record_planted_while_a_holder_takes_the_slots_is_not_read_as_its_own() {
    for admission in [
        Admission::Unmeasured,
        Admission::Measured { peak_rss_bytes: 1 },
    ] {
        let seen = look_during_acquisition(admission);
        assert!(seen.held, "{admission:?}: slot 0 is locked by then");
        assert!(
            !seen.planted_token_holds,
            "{admission:?}: the planted token passed for the holder's"
        );
        assert_ne!(
            seen.reservation.and_then(|r| r.reserved_bytes),
            Some(1),
            "{admission:?}: the planted reservation was read as the holder's"
        );
    }
}

#[test]
fn a_slot_only_looked_at_is_given_back_empty() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path();
    killed_holder_in_slot_1(data);
    let view = MachineView {
        free_memory: &|| Some(u64::MAX),
        group_rss: &|_| None,
        group_state: &|_| None,
    };
    let small = Admission::Measured { peak_rss_bytes: 1 };
    let guard = try_acquire_in(data, 2, small, &view).unwrap().unwrap();
    assert_eq!(guard.index(), 0);
    // Slot 1 was taken for the look and given back: nobody holds it, and no
    // record of a holder that never was stays behind in it.
    assert!(!is_held(data, 1));
    assert_eq!(fs::read_to_string(slot_path(data, 1)).unwrap(), "");
    assert_eq!(fs::read_to_string(token_path(data, 1)).unwrap(), "");
}

#[test]
fn the_record_of_a_guest_that_holds_the_slot_is_still_read() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path();
    let lock = slot_path(data, 0);
    fs::create_dir_all(lock.parent().unwrap()).unwrap();
    fs::write(&lock, "").unwrap();
    // A guest locks the lock file it may only read and writes its record
    // into `guest/`.
    let held = fs::File::open(&lock).unwrap();
    held.try_lock().unwrap();
    plant_guest_record(data, 0);

    let planted = SlotToken::parse(PLANTED).unwrap();
    assert!(holds_token(data, 0, &planted));
    assert_eq!(
        read_reservation(data, 0).and_then(|r| r.reserved_bytes),
        Some(1)
    );
    held.unlock().unwrap();
}
