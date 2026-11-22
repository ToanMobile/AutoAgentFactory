//! A command that runs alone needs every slot idle at once. With two or
//! more slots, measured commands arriving one after another would keep one
//! slot busy for ever; after a wait the loner closes the door behind the
//! commands that are running.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use aaf_domain::ports::Clock;
use tempfile::TempDir;

use super::super::DRAIN_AFTER_SECS;
use super::super::slots::{Admission, Busy, SlotGuard, drain_path, try_acquire, try_drain};
use super::support::Rig;

const SMALL: Admission = Admission::Measured { peak_rss_bytes: 1 };

fn plenty() -> Option<u64> {
    Some(u64::MAX)
}

#[test]
fn draining_refuses_measured_commands_in_every_slot_and_not_the_loner() {
    let data = TempDir::new().unwrap();
    let drain = try_drain(data.path()).unwrap().unwrap();
    // Only one command drains at a time.
    assert!(try_drain(data.path()).unwrap().is_none());

    // Even slot 0 of an idle machine is closed to a measured command.
    let refused = try_acquire(data.path(), 3, SMALL, &plenty).unwrap();
    assert_eq!(refused.unwrap_err(), Busy::SlotsTaken);
    let alone = try_acquire(data.path(), 3, Admission::Unmeasured, &plenty)
        .unwrap()
        .unwrap();
    assert_eq!(alone.index(), 0);

    drop(alone);
    drop(drain);
    let admitted = try_acquire(data.path(), 3, SMALL, &plenty).unwrap();
    assert_eq!(admitted.unwrap().index(), 0);
}

#[test]
fn drain_lock_left_by_a_holder_that_is_gone_blocks_nobody() {
    let data = TempDir::new().unwrap();
    // What a dead drainer leaves: the file, and no lock on it (the kernel
    // drops the lock with the process, as `real_slots` proves for slots).
    drop(try_drain(data.path()).unwrap().unwrap());
    assert!(drain_path(data.path()).exists());

    let first = try_acquire(data.path(), 2, SMALL, &plenty)
        .unwrap()
        .unwrap();
    let second = try_acquire(data.path(), 2, SMALL, &plenty)
        .unwrap()
        .unwrap();
    assert_eq!((first.index(), second.index()), (0, 1));
    assert!(try_drain(data.path()).unwrap().is_some());
}

#[test]
fn loner_is_not_starved_by_measured_commands_that_keep_arriving() {
    const GIVE_UP_AFTER_SLEEPS: usize = 40;
    let data = TempDir::new().unwrap();
    let rig = Rig::calm();
    assert_eq!(rig.throttle().ceiling(), 2);
    let started = rig.clock.now();

    // One measured command runs. On every poll of the waiting loner a new
    // one arrives and takes the free slot, then the oldest one finishes: a
    // slot is busy at every instant.
    let first = try_acquire(data.path(), 2, SMALL, &plenty)
        .unwrap()
        .unwrap();
    let running: Arc<Mutex<VecDeque<SlotGuard>>> = Arc::new(Mutex::new(VecDeque::from([first])));
    let arrivals: Arc<Mutex<Vec<i64>>> = Arc::new(Mutex::new(Vec::new()));
    let (dir, clock) = (data.path().to_path_buf(), Arc::clone(&rig.clock));
    let (queue, admitted_at) = (Arc::clone(&running), Arc::clone(&arrivals));
    rig.sleeper.on_sleep(move |count| {
        let mut queue = queue.lock().unwrap();
        if count > GIVE_UP_AFTER_SLEEPS {
            // The stream ends, so a starved waiter still ends the test.
            queue.clear();
            return;
        }
        if let Ok(arrival) = try_acquire(&dir, 2, SMALL, &plenty).unwrap() {
            admitted_at.lock().unwrap().push(clock.now());
            queue.push_back(arrival);
        }
        if queue.len() > 1 || admitted_at.lock().unwrap().last() != Some(&clock.now()) {
            queue.pop_front();
        }
    });

    let admitted = rig
        .throttle()
        .admit(data.path(), &*rig.clock, "repo", "never_measured")
        .unwrap();
    assert_eq!(admitted.slot.index(), 0);
    assert!(
        rig.sleeper.count() < GIVE_UP_AFTER_SLEEPS,
        "starved for {} polls",
        rig.sleeper.count()
    );
    // Arrivals were let in before the loner started draining, none after.
    let arrivals = arrivals.lock().unwrap().clone();
    let drain_after = i64::try_from(DRAIN_AFTER_SECS).unwrap();
    assert!(!arrivals.is_empty());
    // One poll (at most 10 s of backoff) passes between the threshold and
    // the attempt that takes the drain lock.
    assert!(
        arrivals.iter().all(|at| at - started < drain_after + 10),
        "{arrivals:?}"
    );
    assert!(admitted.waited_secs >= DRAIN_AFTER_SECS);
    // The door is open again as soon as the loner has its slot.
    assert!(try_drain(data.path()).unwrap().is_some());
    assert!(running.lock().unwrap().is_empty());
}
