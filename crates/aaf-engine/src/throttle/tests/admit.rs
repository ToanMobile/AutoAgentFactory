use std::sync::{Arc, Mutex};
use std::time::Duration;

use aaf_domain::ports::Clock;
use tempfile::TempDir;

use super::super::slots::{Admission, try_acquire};
use super::super::stats::{HeavySample, record};
use super::super::{Backoff, Pressure, SlotGuard};
use super::support::{GIB, Rig};

pub(super) const REPO: &str = "repo";
pub(super) const LABEL: &str = "test_full";

/// Marks the command as measured with a 2 GiB peak.
pub(super) fn measure(rig: &Rig, data: &TempDir) {
    let sample = HeavySample {
        peak_rss_bytes: 2 * GIB,
        duration_ms: 10,
    };
    assert!(record(data.path(), REPO, LABEL, sample, &rig.sleeper).unwrap());
}

/// Takes slot 0 the way another `aaf` process would.
pub(super) fn occupy(data: &TempDir) -> SlotGuard {
    let measured = Admission::Measured { peak_rss_bytes: 1 };
    try_acquire(data.path(), 1, measured, &|| None)
        .unwrap()
        .unwrap()
}

#[test]
fn calm_machine_and_free_slot_admit_at_once() {
    let data = TempDir::new().unwrap();
    let rig = Rig::calm();
    let admitted = rig
        .throttle()
        .admit(data.path(), &*rig.clock, REPO, LABEL)
        .unwrap();
    assert_eq!(admitted.slot.index(), 0);
    assert_eq!(admitted.waited_secs, 0);
    assert!(!admitted.degraded);
    assert_eq!(rig.sleeper.count(), 0);
    assert!(rig.events().is_empty(), "{:?}", rig.events());
}

#[test]
fn waits_for_a_taken_slot_with_growing_jittered_delays() {
    let data = TempDir::new().unwrap();
    let mut rig = Rig::calm();
    rig.machine.max_heavy_parallel = 1;
    measure(&rig, &data);
    let held = Arc::new(Mutex::new(Some(occupy(&data))));
    let release = Arc::clone(&held);
    rig.sleeper.on_sleep(move |count| {
        if count == 6 {
            release.lock().unwrap().take();
        }
    });

    let admitted = rig
        .throttle()
        .admit(data.path(), &*rig.clock, REPO, LABEL)
        .unwrap();
    assert_eq!(admitted.slot.index(), 0);
    assert!(!admitted.degraded);
    let slept = rig.sleeper.slept.lock().unwrap().clone();
    assert_eq!(slept.len(), 6);
    // Head-of-queue measured waiter doubles from 250 ms up to FRONT_OF_QUEUE_CAP (1 s), +/-25 %.
    let bases = [250u64, 500, 1_000, 1_000, 1_000, 1_000];
    for (delay, base) in slept.iter().zip(bases) {
        let millis = u64::try_from(delay.as_millis()).unwrap();
        assert!(
            millis >= base * 3 / 4 && millis <= base * 5 / 4,
            "{millis} vs {base}"
        );
    }
    assert!(admitted.waited_secs >= 6);
    assert_eq!(
        admitted.waited_secs,
        u64::try_from(rig.clock.now() - 1_000).unwrap()
    );
    assert!(rig.events_starting("WaitingForSlot") >= 1);
    assert!(rig.events()[0].contains("SlotsTaken"), "{:?}", rig.events());
}

#[test]
fn backoff_is_capped_and_differs_between_seeds() {
    let mut one = Backoff::new(1);
    let mut two = Backoff::new(2);
    let first: Vec<Duration> = (0..12).map(|_| one.next_delay()).collect();
    let second: Vec<Duration> = (0..12).map(|_| two.next_delay()).collect();
    assert_ne!(first, second);
    assert!(first.iter().all(|delay| *delay <= Duration::from_secs(10)));
    assert!(first[11] >= Duration::from_secs(6));
    // Same seed, same sequence: tests stay deterministic.
    let mut again = Backoff::new(1);
    assert_eq!(first[0], again.next_delay());
}

#[test]
fn unmeasured_command_waits_until_it_can_run_alone() {
    let data = TempDir::new().unwrap();
    let rig = Rig::calm();
    let held = Arc::new(Mutex::new(Some(occupy(&data))));
    let release = Arc::clone(&held);
    rig.sleeper.on_sleep(move |count| {
        if count == 2 {
            release.lock().unwrap().take();
        }
    });
    // Two slots and 64 GiB free, but nothing is known about the command.
    let admitted = rig
        .throttle()
        .admit(data.path(), &*rig.clock, REPO, LABEL)
        .unwrap();
    assert_eq!(admitted.slot.index(), 0);
    assert_eq!(rig.sleeper.count(), 2);
    assert!(rig.events()[0].contains("RunsAlone"), "{:?}", rig.events());
}

#[test]
fn measured_command_shares_the_machine_when_memory_allows() {
    let data = TempDir::new().unwrap();
    let rig = Rig::calm();
    measure(&rig, &data);
    let _first = occupy(&data);
    let admitted = rig
        .throttle()
        .admit(data.path(), &*rig.clock, REPO, LABEL)
        .unwrap();
    assert_eq!(admitted.slot.index(), 1);
    assert_eq!(rig.sleeper.count(), 0);
}

#[test]
fn measured_command_waits_while_free_memory_is_short_or_unknown() {
    let data = TempDir::new().unwrap();
    let rig = Rig::calm();
    measure(&rig, &data);
    let _first = occupy(&data);
    // Needs 2.5 GiB: unknown, then too little, then enough.
    rig.probe.free.set(vec![None, Some(2 * GIB), Some(3 * GIB)]);
    let admitted = rig
        .throttle()
        .admit(data.path(), &*rig.clock, REPO, LABEL)
        .unwrap();
    assert_eq!(admitted.slot.index(), 1);
    assert_eq!(rig.sleeper.count(), 2);
    assert!(admitted.waited_secs >= 2);
    let events = rig.events();
    assert!(events[0].contains("free memory"), "{events:?}");
    assert!(events[1].contains("NotEnoughMemory"), "{events:?}");
}

#[test]
fn memory_pressure_is_waited_out_without_a_ceiling() {
    let data = TempDir::new().unwrap();
    let mut rig = Rig::calm();
    // The load ceiling is tiny; pressure must ignore it.
    rig.machine.heavy_wait_secs = 1;
    let mut script = vec![Some(Pressure::Warn); 40];
    script.extend(vec![Some(Pressure::Critical); 40]);
    script.push(Some(Pressure::Normal));
    rig.probe.pressure.set(script);

    let admitted = rig
        .throttle()
        .admit(data.path(), &*rig.clock, REPO, LABEL)
        .unwrap();
    assert_eq!(rig.sleeper.count(), 80);
    assert!(admitted.waited_secs > 80);
    assert!(!admitted.degraded);
    assert!(rig.events_starting("WaitingForMemory") >= 2);
}

#[test]
fn high_load_waits_then_runs_degraded() {
    let data = TempDir::new().unwrap();
    let mut rig = Rig::calm();
    rig.machine.heavy_wait_secs = 60;
    rig.probe.load.set(vec![Some(151)]);

    let admitted = rig
        .throttle()
        .admit(data.path(), &*rig.clock, REPO, LABEL)
        .unwrap();
    assert!(admitted.degraded);
    assert!(admitted.waited_secs >= 60, "{}", admitted.waited_secs);
    // The wait ends at the ceiling, not long after it.
    assert!(admitted.waited_secs < 60 + 10, "{}", admitted.waited_secs);
    let events = rig.events();
    assert!(events[0].starts_with("WaitingForLoad"), "{events:?}");
    assert!(events[0].contains("load_percent: 151"), "{events:?}");
    assert!(events[0].contains("gives_up_after_secs: 60"), "{events:?}");
    // Reported sparingly: at most once per 10 s of waiting.
    assert!(events.len() <= 8, "{events:?}");
}

#[test]
fn load_that_drops_in_time_is_not_degraded() {
    let data = TempDir::new().unwrap();
    let mut rig = Rig::calm();
    rig.machine.heavy_wait_secs = 600;
    rig.probe
        .load
        .set(vec![Some(400), Some(300), Some(151), Some(150)]);
    let admitted = rig
        .throttle()
        .admit(data.path(), &*rig.clock, REPO, LABEL)
        .unwrap();
    assert!(!admitted.degraded);
    // 150 % is the limit itself, not above it.
    assert_eq!(rig.sleeper.count(), 3);
    assert!(admitted.waited_secs >= 3);
}

#[test]
fn zero_max_load_disables_the_load_check_only() {
    let data = TempDir::new().unwrap();
    let mut rig = Rig::calm();
    rig.machine.max_load_percent = 0;
    rig.probe.load.set(vec![Some(5_000)]);
    let admitted = rig
        .throttle()
        .admit(data.path(), &*rig.clock, REPO, LABEL)
        .unwrap();
    assert!(!admitted.degraded);
    assert_eq!(admitted.waited_secs, 0);
    assert_eq!(rig.probe.load.calls(), 0);
    drop(admitted);

    // Memory pressure still holds the command back.
    rig.probe
        .pressure
        .set(vec![Some(Pressure::Warn), Some(Pressure::Normal)]);
    let admitted = rig
        .throttle()
        .admit(data.path(), &*rig.clock, REPO, LABEL)
        .unwrap();
    assert_eq!(rig.sleeper.count(), 1);
    assert!(admitted.waited_secs >= 1);
}

#[test]
fn unreadable_load_and_pressure_count_as_normal_with_a_warning() {
    let data = TempDir::new().unwrap();
    let rig = Rig::calm();
    rig.probe.load.set(vec![None]);
    rig.probe.pressure.set(vec![None]);
    let admitted = rig
        .throttle()
        .admit(data.path(), &*rig.clock, REPO, LABEL)
        .unwrap();
    assert_eq!(admitted.waited_secs, 0);
    assert!(!admitted.degraded);
    assert_eq!(
        rig.events(),
        vec![
            "ProbeUnreadable { what: \"memory pressure\" }",
            "ProbeUnreadable { what: \"CPU load\" }"
        ]
    );
}

#[test]
fn custom_labeled_command_reuses_cmd_family_peak_instead_of_running_alone() {
    let data = TempDir::new().unwrap();
    let rig = Rig::calm();
    let sample = HeavySample {
        peak_rss_bytes: 2 * GIB,
        duration_ms: 1_000,
    };
    assert!(
        record(
            data.path(),
            REPO,
            &super::super::stats::cmd_label("cargo"),
            sample,
            &rig.sleeper,
        )
        .unwrap()
    );
    let _first = occupy(&data);
    let ask = super::super::Ask {
        repo_id: REPO,
        label: "fix-g6",
        tool: None,
        cmd_family: Some("cargo"),
        exclusive: false,
        wait_limit_secs: None,
    };
    let admitted = rig
        .throttle()
        .admit_ask(data.path(), &*rig.clock, &ask)
        .unwrap()
        .unwrap();
    assert_eq!(admitted.slot.index(), 1);
    assert_eq!(rig.sleeper.count(), 0);
}

#[test]
fn newer_measured_waiter_yields_to_older_fifo_ticket_when_slots_are_scarce() {
    let data = TempDir::new().unwrap();
    let rig = Rig::calm();
    measure(&rig, &data);
    // 1 of 2 slots occupied, leaving 1 free slot, with 1 older waiter queued.
    let _busy = occupy(&data);
    let older = super::super::queue::WaitTicket::join(data.path(), 500).unwrap();

    let ask = super::super::Ask {
        repo_id: REPO,
        label: LABEL,
        tool: None,
        cmd_family: None,
        exclusive: false,
        wait_limit_secs: Some(0),
    };
    // Newcomer (clock = 1000 > 500) must yield instead of jumping ahead.
    let jumped = rig
        .throttle()
        .admit_ask(data.path(), &*rig.clock, &ask)
        .unwrap();
    assert!(jumped.is_none(), "newcomer must yield to older waiter");

    // Once the older waiter drops its ticket, the newcomer is admitted.
    drop(older);
    let admitted = rig
        .throttle()
        .admit_ask(data.path(), &*rig.clock, &ask)
        .unwrap();
    assert!(admitted.is_some());
}

#[test]
fn high_loadavg_with_calm_real_cpu_admits_immediately() {
    let data = TempDir::new().unwrap();
    let rig = Rig::calm();
    // macOS I/O-inflated loadavg (172% > 150%), but real CPU busy is only 55% (< 85%).
    rig.probe.load.set(vec![Some(172)]);
    rig.probe.cpu_busy.set(vec![Some(55)]);

    let admitted = rig
        .throttle()
        .admit(data.path(), &*rig.clock, REPO, LABEL)
        .unwrap();
    assert!(!admitted.degraded);
    assert_eq!(admitted.waited_secs, 0);
    assert_eq!(rig.sleeper.count(), 0);
    assert_eq!(rig.probe.cpu_busy.calls(), 1);
}

#[test]
fn high_loadavg_and_saturated_real_cpu_waits_until_cpu_drops() {
    let data = TempDir::new().unwrap();
    let rig = Rig::calm();
    rig.probe.load.set(vec![Some(180)]);
    rig.probe.cpu_busy.set(vec![Some(92), Some(85), Some(60)]);

    let admitted = rig
        .throttle()
        .admit(data.path(), &*rig.clock, REPO, LABEL)
        .unwrap();
    assert!(!admitted.degraded);
    assert_eq!(rig.sleeper.count(), 2);
    let events = rig.events();
    assert!(
        events[0].contains("cpu_busy_percent: Some(92)"),
        "{events:?}"
    );
    assert!(
        events[0].contains("max_cpu_percent: Some(85)"),
        "{events:?}"
    );
}

#[test]
fn calm_loadavg_never_queries_slow_cpu_busy_probe() {
    let data = TempDir::new().unwrap();
    let rig = Rig::calm();
    rig.probe.load.set(vec![Some(120)]);
    rig.probe.cpu_busy.set(vec![Some(99)]);

    let admitted = rig
        .throttle()
        .admit(data.path(), &*rig.clock, REPO, LABEL)
        .unwrap();
    assert!(!admitted.degraded);
    assert_eq!(rig.probe.cpu_busy.calls(), 0);
}
