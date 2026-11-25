//! Oracle for AUTO-038: a live orphaned heavy-slot process group whose lock
//! file `mtime` is older than `heavy_wait_secs` (because the command ran for a
//! long time before `aaf` crashed) must NOT be prematurely reaped on a fresh
//! waiter's admission attempt before `heavy_wait_secs` of orphan wait has
//! elapsed.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs::{self, File};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use aaf_domain::config::MachineConfig;
use aaf_domain::ports::Clock;
use aaf_engine::throttle::paths::token_path;
use aaf_engine::throttle::probe::Unreadable;
use aaf_engine::throttle::slots::slot_path;
use aaf_engine::throttle::stats::{HeavySample, record};
use aaf_engine::throttle::{
    Ask, Busy, GroupControl, GroupState, LoadProbe, Pressure, PriorityTools, Progress, Sleeper,
    Throttle,
};
use tempfile::TempDir;

const ORPHAN_PGID: u32 = 42_424;
const ORPHAN_STARTED: &str = "Sun Nov 22 11:00:00 2026";
const ORPHAN_TOKEN: &str = "0123456789abcdef0123456789abcdef";
const HEAVY_WAIT_SECS: u64 = 300;
const BACKDATE_SECS: u64 = 1_200;
const SHORT_BUDGET_SECS: u64 = 10;

struct TestClock(AtomicI64);

impl Clock for TestClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct AdvancingSleeper {
    clock: Arc<TestClock>,
    calls: AtomicUsize,
}

impl Sleeper for AdvancingSleeper {
    fn sleep(&self, duration: Duration) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let secs = i64::try_from(duration.as_secs().max(1)).unwrap();
        self.clock.0.fetch_add(secs, Ordering::SeqCst);
    }
}

struct CalmProbe;

impl LoadProbe for CalmProbe {
    fn load_percent(&self) -> Option<u32> {
        Some(10)
    }

    fn pressure(&self) -> Option<Pressure> {
        Some(Pressure::Normal)
    }

    fn free_memory_bytes(&self) -> Option<u64> {
        Some(64 * 1024 * 1024 * 1024)
    }
}

struct LiveOrphanGroup {
    pgid: u32,
    started: &'static str,
}

impl GroupControl for LiveOrphanGroup {
    fn rss_bytes(&self, _pgid: u32) -> Option<u64> {
        None
    }

    fn stop(&self, _pgid: u32) -> bool {
        false
    }

    fn resume(&self, _pgid: u32) -> bool {
        false
    }

    fn group_state(&self, pgid: u32) -> Option<GroupState> {
        (pgid == self.pgid).then(|| GroupState {
            stopped: false,
            leader_started: Some(self.started.to_string()),
        })
    }

    fn read_group_state(&self, pgid: u32) -> Result<Option<GroupState>, Unreadable> {
        Ok(self.group_state(pgid))
    }
}

/// Plants an unlocked (orphaned) slot 0 record for a still-running command and
/// backdates `heavy.0.lock` mtime well past `heavy_wait_secs` to simulate a
/// long-running build whose `aaf` wrapper died only moments ago.
fn plant_old_live_orphan_slot(data_dir: &TempDir) {
    let lock = slot_path(data_dir.path(), 0);
    let token = token_path(data_dir.path(), 0);
    fs::create_dir_all(lock.parent().unwrap()).unwrap();
    fs::create_dir_all(token.parent().unwrap()).unwrap();
    fs::write(&token, format!("token={ORPHAN_TOKEN}\n")).unwrap();
    fs::write(
        &lock,
        format!("reserved=unknown\npgid={ORPHAN_PGID}\nlstart={ORPHAN_STARTED}\n"),
    )
    .unwrap();

    let old_mtime = SystemTime::now()
        .checked_sub(Duration::from_secs(BACKDATE_SECS))
        .unwrap();
    File::options()
        .write(true)
        .open(&lock)
        .unwrap()
        .set_modified(old_mtime)
        .unwrap();
}

fn assert_live_orphan_not_reaped_within_short_budget(measured: bool) {
    let data = TempDir::new().unwrap();
    plant_old_live_orphan_slot(&data);

    let clock = Arc::new(TestClock(AtomicI64::new(1_000)));
    let sleeper = AdvancingSleeper {
        clock: Arc::clone(&clock),
        calls: AtomicUsize::new(0),
    };
    if measured {
        let sample = HeavySample {
            peak_rss_bytes: 512 * 1024 * 1024,
            duration_ms: 100,
        };
        assert!(record(data.path(), "repo-auto038", "test_full", sample, &sleeper).unwrap());
    }

    let machine = MachineConfig {
        max_heavy_parallel: 1,
        heavy_wait_secs: HEAVY_WAIT_SECS,
        ..MachineConfig::default()
    };
    let probe = CalmProbe;
    let control = LiveOrphanGroup {
        pgid: ORPHAN_PGID,
        started: ORPHAN_STARTED,
    };
    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let progress = move |event: &Progress<'_>| {
        sink.lock().unwrap().push(format!("{event:?}"));
    };

    let throttle = Throttle {
        machine: &machine,
        probe: &probe,
        sleeper: &sleeper,
        control: &control,
        progress: &progress,
        cpus: 4,
        tools: PriorityTools::default(),
        jitter_seed: 7,
        sample_every: Duration::from_secs(2),
    };

    let ask = Ask {
        repo_id: "repo-auto038",
        label: "test_full",
        tool: None,
        cmd_family: None,
        exclusive: false,
        wait_limit_secs: Some(SHORT_BUDGET_SECS),
    };

    let admitted = throttle.admit_ask(data.path(), &*clock, &ask).unwrap();
    let lock_after = fs::read_to_string(slot_path(data.path(), 0)).unwrap();
    let token_after = fs::read_to_string(token_path(data.path(), 0)).unwrap();

    assert!(
        admitted.is_none(),
        "AUTO-038: command B must NOT be admitted within {SHORT_BUDGET_SECS}s (< heavy_wait_secs={HEAVY_WAIT_SECS}s) \
         while live orphan pgid={ORPHAN_PGID} owns slot 0 (even though lock mtime is {BACKDATE_SECS}s old); \
         got admitted={admitted:?}, lock_after={lock_after:?}, sleeps={}",
        sleeper.calls.load(Ordering::SeqCst)
    );
    assert!(
        lock_after.contains(&format!("pgid={ORPHAN_PGID}")),
        "AUTO-038: live orphan pgid record in heavy.0.lock must remain intact; got {lock_after:?}"
    );
    assert!(
        lock_after.contains(&format!("lstart={ORPHAN_STARTED}")),
        "AUTO-038: live orphan lstart record in heavy.0.lock must remain intact; got {lock_after:?}"
    );
    assert!(
        token_after.contains(ORPHAN_TOKEN),
        "AUTO-038: live orphan token in heavy.0.token must remain intact; got {token_after:?}"
    );
    let recorded_events = events.lock().unwrap();
    assert!(
        recorded_events
            .iter()
            .any(|ev| ev.contains(&format!("{:?}", Busy::Orphaned { pgid: ORPHAN_PGID }))),
        "expected WaitingForSlot(Busy::Orphaned {{ pgid: {ORPHAN_PGID} }}), got {recorded_events:?}"
    );
}

#[test]
fn auto038_live_orphan_with_old_mtime_is_not_reaped_on_short_wait_unmeasured() {
    assert_live_orphan_not_reaped_within_short_budget(false);
}

#[test]
fn auto038_live_orphan_with_old_mtime_is_not_reaped_on_short_wait_measured() {
    assert_live_orphan_not_reaped_within_short_budget(true);
}
