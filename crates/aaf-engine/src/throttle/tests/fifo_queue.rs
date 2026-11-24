//! Admission order between FIFO waiters: front-of-queue polling caps,
//! ticketless guests, and the millisecond wait limit.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use aaf_domain::ports::Clock;
use tempfile::TempDir;

use super::admit::{LABEL, REPO, measure, occupy};
use super::support::Rig;

#[test]
fn fifo_waiter_behind_older_ticket_polls_at_front_of_queue_cap() {
    let data = TempDir::new().unwrap();
    let mut rig = Rig::calm();
    rig.machine.max_heavy_parallel = 1;
    measure(&rig, &data);
    let held = Arc::new(Mutex::new(Some(occupy(&data))));
    let older = Arc::new(Mutex::new(Some(
        super::super::queue::WaitTicket::join(data.path(), 500).unwrap(),
    )));
    let release_slot = Arc::clone(&held);
    let release_ticket = Arc::clone(&older);
    rig.sleeper.on_sleep(move |count| {
        if count == 6 {
            release_ticket.lock().unwrap().take();
            release_slot.lock().unwrap().take();
        }
    });

    let admitted = rig
        .throttle()
        .admit(data.path(), &*rig.clock, REPO, LABEL)
        .unwrap();
    assert_eq!(admitted.slot.index(), 0);
    let slept = rig.sleeper.slept.lock().unwrap().clone();
    assert_eq!(slept.len(), 6);
    // A waiter behind an older FIFO ticket (`ahead > 0`) may be the next
    // head at any moment: it polls no slower than the head (audit a13
    // O11-4/E10-F4: an 8 s cap left a freed slot idle for up to 10 s).
    let bases = [250u64, 500, 1_000, 1_000, 1_000, 1_000];
    for (delay, base) in slept.iter().zip(bases) {
        let millis = u64::try_from(delay.as_millis()).unwrap();
        assert!(millis <= 2_500, "{millis} ms over the 2.5 s bound");
        assert!(
            millis >= base * 3 / 4 && millis <= base * 5 / 4,
            "{millis} vs {base}"
        );
    }
}

#[test]
fn draining_job_caps_wakeup_at_front_of_queue_cap_and_immediate_admit_skips_phase_c() {
    let data = TempDir::new().unwrap();
    let rig = Rig::calm();
    // Calm immediate admit skips Phase C pressure re-probe (only Phase A polls once).
    let admitted = rig
        .throttle()
        .admit(data.path(), &*rig.clock, REPO, LABEL)
        .unwrap();
    assert_eq!(rig.probe.pressure.calls(), 1);
    drop(admitted);

    // Unmeasured job waiting past DRAIN_AFTER_SECS grows to 8 s before draining,
    // then drops to FRONT_OF_QUEUE_CAP (<= 1.25 s) once `drain.is_some()`.
    let rig = Rig::calm();
    let held = Arc::new(Mutex::new(Some(occupy(&data))));
    let release = Arc::clone(&held);
    let clock = Arc::clone(&rig.clock);
    let started = clock.now();
    let drain_after = i64::try_from(super::super::DRAIN_AFTER_SECS).unwrap();
    let draining_sleeps = Arc::new(Mutex::new(0usize));
    let draining_ref = Arc::clone(&draining_sleeps);
    rig.sleeper.on_sleep(move |_| {
        if clock.now() - started >= drain_after {
            let mut n = draining_ref.lock().unwrap();
            *n += 1;
            if *n >= 4 {
                release.lock().unwrap().take();
            }
        }
    });

    let admitted = rig
        .throttle()
        .admit(data.path(), &*rig.clock, REPO, "unmeasured_drain")
        .unwrap();
    assert_eq!(admitted.slot.index(), 0);
    let slept = rig.sleeper.slept.lock().unwrap().clone();
    assert!(slept.len() >= 8, "{slept:?}");
    // Before draining, delays exceed FRONT_OF_QUEUE_CAP * 1.25 (reach ~8 s).
    assert!(
        slept.iter().any(|d| *d >= Duration::from_secs(4)),
        "expected pre-drain backoff to grow past 4 s: {slept:?}"
    );
    // Final sleeps while draining are all capped at FRONT_OF_QUEUE_CAP * 1.25 (1250 ms).
    for delay in &slept[slept.len() - 3..] {
        assert!(
            *delay <= Duration::from_millis(1_250) && *delay >= Duration::from_millis(750),
            "expected draining delay in 750..=1250ms, got {delay:?} in {slept:?}"
        );
    }
}

/// A sandboxed guest cannot take a ticket, so it compares by its arrival
/// time: a host ticket taken after the guest arrived is behind it, not ahead
/// (audit a13 L1-1: guests yielded to every newer host waiter and starved).
#[cfg(unix)]
#[test]
fn ticketless_guest_yields_only_to_host_tickets_older_than_its_arrival() {
    use std::os::unix::fs::PermissionsExt;

    use super::super::paths::{drain_path, is_guest};
    use super::super::queue::WaitTicket;
    use super::super::slots::prepare_for_guests;

    let data = TempDir::new().unwrap();
    let rig = Rig::calm();
    prepare_for_guests(data.path(), 2, &|_| None).unwrap();
    // 1 of 2 slots taken by a host: one slot free for one waiter.
    let _busy = occupy(&data);
    let ask = super::super::Ask {
        repo_id: REPO,
        label: LABEL,
        tool: None,
        cmd_family: None,
        exclusive: false,
        wait_limit_secs: Some(0),
    };
    let arrival = rig.clock.now();
    let as_guest = |host_ticket_at: i64| {
        let host = WaitTicket::join(data.path(), host_ticket_at).unwrap();
        let drain = drain_path(data.path());
        std::fs::set_permissions(&drain, std::fs::Permissions::from_mode(0o400)).unwrap();
        assert!(is_guest(data.path()));
        // A guest keeps its own measurements (`guest/`).
        measure(&rig, &data);
        let got = rig
            .throttle()
            .admit_ask(data.path(), &*rig.clock, &ask)
            .unwrap();
        std::fs::set_permissions(&drain, std::fs::Permissions::from_mode(0o600)).unwrap();
        drop(host);
        got.is_some()
    };
    assert!(
        as_guest(arrival + 5),
        "a host that queued after the guest arrived must not hold it back"
    );
    assert!(
        !as_guest(arrival - 5),
        "the guest still yields to a host that queued before it arrived"
    );
}

/// A clock and sleeper that keep milliseconds; the clock reads whole seconds
/// like the real one.
struct MillisTime {
    millis: std::sync::atomic::AtomicU64,
    slept: Mutex<Vec<Duration>>,
}

impl Clock for MillisTime {
    fn now(&self) -> i64 {
        let millis = self.millis.load(std::sync::atomic::Ordering::SeqCst);
        i64::try_from(millis / 1_000).unwrap()
    }
}

impl super::super::Sleeper for MillisTime {
    fn sleep(&self, duration: Duration) {
        self.slept.lock().unwrap().push(duration);
        let millis = u64::try_from(duration.as_millis()).unwrap();
        self.millis
            .fetch_add(millis, std::sync::atomic::Ordering::SeqCst);
    }
}

/// `--timeout 1` with the slot held: the wait ends neither before one second
/// has passed (the second turning 100 ms after the start ended it at once)
/// nor a whole backoff sleep after it (audit a13 E10-F3).
#[test]
fn wait_limit_is_kept_to_the_millisecond() {
    for start_millis in [1_000_000u64, 1_000_900, 1_000_999] {
        let data = TempDir::new().unwrap();
        let mut rig = Rig::calm();
        rig.machine.max_heavy_parallel = 1;
        measure(&rig, &data);
        let _held = occupy(&data);
        let time = MillisTime {
            millis: std::sync::atomic::AtomicU64::new(start_millis),
            slept: Mutex::new(Vec::new()),
        };
        let mut throttle = rig.throttle();
        throttle.sleeper = &time;
        let ask = super::super::Ask {
            repo_id: REPO,
            label: LABEL,
            tool: None,
            cmd_family: None,
            exclusive: false,
            wait_limit_secs: Some(1),
        };
        let got = throttle.admit_ask(data.path(), &time, &ask).unwrap();
        assert!(got.is_none());
        let slept: Duration = time.slept.lock().unwrap().iter().sum();
        assert!(
            slept >= Duration::from_secs(1),
            "gave up after {slept:?} (start {start_millis} ms)"
        );
        assert!(
            slept <= Duration::from_millis(1_250),
            "overshot to {slept:?} (start {start_millis} ms)"
        );
    }
}
