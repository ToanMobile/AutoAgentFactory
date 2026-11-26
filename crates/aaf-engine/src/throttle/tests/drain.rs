//! A command that runs alone needs every slot idle at once. With two or
//! more slots, measured commands arriving one after another would keep one
//! slot busy for ever; after a wait the loner closes the door behind the
//! commands that are running.

use std::collections::VecDeque;
use std::fs;
use std::sync::{Arc, Mutex};

use aaf_domain::ports::Clock;
use tempfile::TempDir;

use super::super::paths::token_path;
use super::super::probe::{GroupState, Unreadable};
use super::super::slots::{
    Admission, Busy, MachineView, SlotGuard, drain_path, slot_path, try_acquire,
    try_acquire_reading, try_drain,
};
use super::super::{Ask, DRAIN_AFTER_SECS, GroupControl, Throttle};
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
    assert_eq!(refused.unwrap_err(), Busy::Draining);
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

#[test]
fn host_reaps_dead_orphan_slot_even_while_drain_lock_is_held() {
    let data = TempDir::new().unwrap();
    let drain = try_drain(data.path()).unwrap().unwrap();

    let lock = slot_path(data.path(), 1);
    let token = token_path(data.path(), 1);
    fs::create_dir_all(lock.parent().unwrap()).unwrap();
    fs::create_dir_all(token.parent().unwrap()).unwrap();
    let record = "reserved=1024\npgid=4242\nlstart=Sun Nov 22 11:00:00 2026\n";
    fs::write(&lock, record).unwrap();
    fs::write(&token, "token=0123456789abcdef0123456789abcdef\n").unwrap();

    let view = MachineView {
        free_memory: &plenty,
        group_rss: &|_| None,
        group_state: &|_| None,
    };

    // Safety invariant: while the orphan group is still alive OR `ps` is unreadable,
    // a draining probe must never wipe the slot record.
    let alive = |pgid: u32| {
        Ok((pgid == 4242).then(|| GroupState {
            leader_started: Some("Sun Nov 22 11:00:00 2026".to_string()),
            stopped: false,
        }))
    };
    assert_eq!(
        try_acquire_reading(data.path(), 2, SMALL, &view, &alive)
            .unwrap()
            .unwrap_err(),
        Busy::Draining
    );
    assert_eq!(fs::read_to_string(&lock).unwrap(), record);
    assert_eq!(
        try_acquire_reading(data.path(), 2, SMALL, &view, &|_| Err(Unreadable))
            .unwrap()
            .unwrap_err(),
        Busy::Draining
    );
    assert_eq!(fs::read_to_string(&lock).unwrap(), record);

    // Once the orphan process group is confirmed dead (`Ok(None)`), the host
    // measured probe still returns `Busy::Draining` (preserving drain fairness)
    // AND clears the dead orphan record so a draining sandboxed guest unblocks.
    assert_eq!(
        try_acquire_reading(data.path(), 2, SMALL, &view, &|_| Ok(None))
            .unwrap()
            .unwrap_err(),
        Busy::Draining
    );
    assert_eq!(fs::read_to_string(&lock).unwrap(), "");
    assert_eq!(fs::read_to_string(&token).unwrap(), "");
    drop(drain);
}

struct UnreadableGroup;

impl GroupControl for UnreadableGroup {
    fn rss_bytes(&self, _pgid: u32) -> Option<u64> {
        None
    }

    fn stop(&self, _pgid: u32) -> bool {
        false
    }

    fn resume(&self, _pgid: u32) -> bool {
        false
    }

    fn read_group_state(&self, _pgid: u32) -> Result<Option<GroupState>, Unreadable> {
        Err(Unreadable)
    }
}

#[test]
fn unmeasured_waiter_releases_drain_lock_when_only_unreadable_orphan_remains() {
    let data = TempDir::new().unwrap();
    let rig = Rig::calm();

    // Slot 0 is actively held by a live measured job; slot 1 holds a dead
    // orphan record that an unreadable-`ps` sandboxed guest cannot verify.
    let live_slot = try_acquire(data.path(), 2, SMALL, &plenty)
        .unwrap()
        .unwrap();
    let orphan_lock = slot_path(data.path(), 1);
    fs::write(
        &orphan_lock,
        "reserved=1024\npgid=4242\nlstart=Sun Nov 22 11:00:00 2026\n",
    )
    .unwrap();

    let held = Arc::new(Mutex::new(Some(live_slot)));
    let saw_drain_held = Arc::new(Mutex::new(false));
    let saw_drain_released = Arc::new(Mutex::new(false));
    let (dir, clock, started) = (
        data.path().to_path_buf(),
        Arc::clone(&rig.clock),
        rig.clock.now(),
    );
    let (held_ref, held_flag, rel_flag) = (
        Arc::clone(&held),
        Arc::clone(&saw_drain_held),
        Arc::clone(&saw_drain_released),
    );
    rig.sleeper.on_sleep(move |_| {
        let elapsed = u64::try_from(clock.now() - started).unwrap_or(0);
        if elapsed >= DRAIN_AFTER_SECS && !*held_flag.lock().unwrap() {
            if try_drain(&dir).unwrap().is_none() {
                *held_flag.lock().unwrap() = true;
                // Now the live job finishes; zero slots are flock-held, only
                // the unreadable orphan record in slot 1 remains.
                held_ref.lock().unwrap().take();
            }
        } else if *held_flag.lock().unwrap() && try_drain(&dir).unwrap().is_some() {
            *rel_flag.lock().unwrap() = true;
        }
    });

    let group = UnreadableGroup;
    let progress = |_: &super::super::Progress<'_>| {};
    let throttle = Throttle {
        machine: &rig.machine,
        probe: &rig.probe,
        sleeper: &rig.sleeper,
        control: &group,
        progress: &progress,
        cpus: rig.cpus,
        tools: rig.tools.clone(),
        jitter_seed: 7,
        sample_every: rig.sample_every,
    };
    let ask = Ask {
        repo_id: "repo",
        label: "unmeasured_guest",
        tool: None,
        cmd_family: None,
        exclusive: false,
        wait_limit_secs: Some(DRAIN_AFTER_SECS + 30),
    };
    let res = throttle.admit_ask(data.path(), &*rig.clock, &ask).unwrap();
    assert!(res.is_none());
    assert!(
        *saw_drain_held.lock().unwrap(),
        "expected waiter to acquire drain.lock while live slot 0 was held"
    );
    assert!(
        *saw_drain_released.lock().unwrap(),
        "expected waiter to drop drain.lock once zero slots remained flock-held"
    );
}
