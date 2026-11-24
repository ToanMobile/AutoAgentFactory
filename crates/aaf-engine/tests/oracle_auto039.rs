#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use aaf_domain::config::MachineConfig;
use aaf_domain::ports::Clock;
use aaf_engine::throttle::paths::{slot_path, token_path};
use aaf_engine::throttle::probe::{GroupState, Unreadable};
use aaf_engine::throttle::stats::{HeavySample, record};
use aaf_engine::throttle::{
    Admitted, Ask, GroupControl, LoadProbe, Pressure, PriorityTools, Progress, SAMPLE_EVERY,
    Sleeper, Throttle,
};
use tempfile::TempDir;

const ORPHAN_PGID: u32 = 42_424;
const ORPHAN_STARTED: &str = "Tue Nov 24 10:00:00 2026";
const ORPHAN_TOKEN: &str = "0123456789abcdef0123456789abcdef";

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

struct FakeClock(AtomicI64);

impl Clock for FakeClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct SteppingSleeper<'a>(&'a FakeClock);

impl Sleeper for SteppingSleeper<'_> {
    fn sleep(&self, duration: Duration) {
        let secs = i64::try_from(duration.as_secs().max(1)).unwrap();
        self.0.0.fetch_add(secs, Ordering::SeqCst);
    }
}

struct ScriptedGroup {
    target_pgid: u32,
    unreadable: bool,
}

impl GroupControl for ScriptedGroup {
    fn rss_bytes(&self, _pgid: u32) -> Option<u64> {
        None
    }

    fn stop(&self, _pgid: u32) -> bool {
        false
    }

    fn resume(&self, _pgid: u32) -> bool {
        false
    }

    fn read_group_state(&self, pgid: u32) -> Result<Option<GroupState>, Unreadable> {
        if pgid == self.target_pgid && self.unreadable {
            Err(Unreadable)
        } else {
            Ok(None)
        }
    }
}

fn quiet(_: &Progress<'_>) {}

fn write_orphan_slot(data_dir: &Path) -> String {
    let lock = slot_path(data_dir, 0);
    let token = token_path(data_dir, 0);
    fs::create_dir_all(lock.parent().unwrap()).unwrap();
    fs::create_dir_all(token.parent().unwrap()).unwrap();
    let body = format!("reserved=unknown\npgid={ORPHAN_PGID}\nlstart={ORPHAN_STARTED}\n");
    fs::write(&lock, &body).unwrap();
    fs::write(&token, format!("token={ORPHAN_TOKEN}\n")).unwrap();
    body
}

fn write_legacy_orphan_slot(data_dir: &Path) -> String {
    let legacy = data_dir.join("locks").join("heavy.0.lock");
    fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    let body = format!(
        "token={ORPHAN_TOKEN}\nreserved=unknown\npgid={ORPHAN_PGID}\nlstart={ORPHAN_STARTED}\n"
    );
    fs::write(&legacy, &body).unwrap();
    body
}

fn admit_short(data_dir: &Path, label: &str, unreadable: bool) -> Option<Admitted> {
    let machine = MachineConfig {
        max_heavy_parallel: 1,
        heavy_wait_secs: 300,
        ..MachineConfig::default()
    };
    let clock = FakeClock(AtomicI64::new(1_000));
    let sleeper = SteppingSleeper(&clock);
    let control = ScriptedGroup {
        target_pgid: ORPHAN_PGID,
        unreadable,
    };
    let throttle = Throttle {
        machine: &machine,
        probe: &CalmProbe,
        sleeper: &sleeper,
        control: &control,
        progress: &quiet,
        cpus: 4,
        tools: PriorityTools::default(),
        jitter_seed: 1,
        sample_every: SAMPLE_EVERY,
    };
    let ask = Ask {
        repo_id: "repo",
        label,
        tool: None,
        cmd_family: None,
        exclusive: false,
        wait_limit_secs: Some(3),
    };
    throttle.admit_ask(data_dir, &clock, &ask).unwrap()
}

#[test]
fn auto039_unreadable_ps_keeps_orphan_slot_busy_and_unclobbered_unmeasured() {
    let data = TempDir::new().unwrap();
    let expected = write_orphan_slot(data.path());

    let admitted = admit_short(data.path(), "build", true);
    assert!(
        admitted.is_none(),
        "slot 0 must stay busy (Ok(None)) when read_group_state({ORPHAN_PGID}) returns Err(Unreadable), got {:?}",
        admitted.map(|a| a.slot.index())
    );

    let lock_after = fs::read_to_string(slot_path(data.path(), 0)).unwrap();
    assert_eq!(
        lock_after, expected,
        "heavy.0.lock orphan record must not be overwritten or cleared when ps is unreadable"
    );
    let token_after = fs::read_to_string(token_path(data.path(), 0)).unwrap();
    assert!(
        token_after.contains(ORPHAN_TOKEN),
        "heavy.0.token must remain intact when ps is unreadable, got {token_after:?}"
    );
}

#[test]
fn auto039_unreadable_ps_keeps_orphan_slot_busy_and_unclobbered_measured() {
    let data = TempDir::new().unwrap();
    let expected = write_orphan_slot(data.path());

    let clock = FakeClock(AtomicI64::new(1_000));
    let sleeper = SteppingSleeper(&clock);
    let sample = HeavySample {
        peak_rss_bytes: 64 * 1024 * 1024,
        duration_ms: 100,
    };
    assert!(record(data.path(), "repo", "test_scoped", sample, &sleeper).unwrap());

    let admitted = admit_short(data.path(), "test_scoped", true);
    assert!(
        admitted.is_none(),
        "measured admission must treat slot 0 as busy when read_group_state({ORPHAN_PGID}) returns Err(Unreadable), got {:?}",
        admitted.map(|a| a.slot.index())
    );

    let lock_after = fs::read_to_string(slot_path(data.path(), 0)).unwrap();
    assert_eq!(
        lock_after, expected,
        "measured admission must not overwrite or clear heavy.0.lock when ps is unreadable"
    );
}

#[test]
fn auto039_unreadable_ps_keeps_legacy_orphan_slot_busy_and_untruncated() {
    let data = TempDir::new().unwrap();
    let expected = write_legacy_orphan_slot(data.path());

    let admitted = admit_short(data.path(), "build", true);
    assert!(
        admitted.is_none(),
        "slot 0 must stay busy when legacy lock holds orphan {ORPHAN_PGID} and read_group_state returns Err(Unreadable), got {:?}",
        admitted.map(|a| a.slot.index())
    );

    let legacy_after = fs::read_to_string(data.path().join("locks").join("heavy.0.lock")).unwrap();
    assert_eq!(
        legacy_after, expected,
        "legacy locks/heavy.0.lock orphan record must not be truncated when ps is unreadable"
    );
}

#[test]
fn auto039_confirmed_dead_orphan_slot_is_still_admitted() {
    let data = TempDir::new().unwrap();
    let _ = write_orphan_slot(data.path());

    let admitted = admit_short(data.path(), "build", false);
    assert_eq!(
        admitted.map(|a| a.slot.index()),
        Some(0),
        "when read_group_state({ORPHAN_PGID}) confirms Ok(None), slot 0 must be reclaimed"
    );
}
