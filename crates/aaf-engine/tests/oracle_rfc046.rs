//! Oracle for RFC-046: persisted `orphaned_since` sidecar in `locks/` (0700/0600),
//! keyed by `(slot_index, pgid, lstart)`, measuring orphan age via `Clock` across
//! short-lived `admit_ask` calls without unlinking `heavy.<n>.lock`, resetting on
//! `(pgid, lstart)` change, preserving state on `Err(Unreadable)`, and cleaning up
//! on reap, claim, and release.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aaf_domain::config::MachineConfig;
use aaf_domain::ports::Clock;
use aaf_engine::throttle::paths::{orphan_since_path, slot_path, token_path};
use aaf_engine::throttle::probe::{GroupState, Unreadable};
use aaf_engine::throttle::slots::{Admission, MachineView, try_acquire_in};
use aaf_engine::throttle::stats::{HeavySample, record};
use aaf_engine::throttle::{
    Ask, Busy, GroupControl, LoadProbe, Pressure, PriorityTools, SAMPLE_EVERY, Sleeper, Throttle,
};
use tempfile::TempDir;

const PGID_A: u32 = 42_424;
const PGID_B: u32 = 55_555;
const LSTART_A: &str = "Sun Nov 22 11:00:00 2026";
const LSTART_B: &str = "Sun Nov 22 12:00:00 2026";
const TOKEN: &str = "0123456789abcdef0123456789abcdef";

struct FakeClock(AtomicI64);

impl Clock for FakeClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct AdvancingSleeper {
    clock: Arc<FakeClock>,
}

impl Sleeper for AdvancingSleeper {
    fn sleep(&self, duration: Duration) {
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

enum ControlMode {
    Live { pgid: u32, lstart: String },
    Unreadable { pgid: u32 },
}

struct ScriptedControl {
    mode: Mutex<ControlMode>,
}

impl ScriptedControl {
    fn live(pgid: u32, lstart: &str) -> Self {
        Self {
            mode: Mutex::new(ControlMode::Live {
                pgid,
                lstart: lstart.to_string(),
            }),
        }
    }

    fn unreadable(pgid: u32) -> Self {
        Self {
            mode: Mutex::new(ControlMode::Unreadable { pgid }),
        }
    }

    fn set_live(&self, pgid: u32, lstart: &str) {
        *self.mode.lock().unwrap() = ControlMode::Live {
            pgid,
            lstart: lstart.to_string(),
        };
    }
}

impl GroupControl for ScriptedControl {
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
        let mode = self.mode.lock().unwrap();
        match &*mode {
            ControlMode::Live {
                pgid: target,
                lstart,
            } => Ok((pgid == *target).then(|| GroupState {
                stopped: false,
                leader_started: Some(lstart.clone()),
            })),
            ControlMode::Unreadable { pgid: target } => {
                if pgid == *target {
                    Err(Unreadable)
                } else {
                    Ok(None)
                }
            }
        }
    }
}

fn sidecar_path(data_dir: &Path, slot: usize) -> PathBuf {
    orphan_since_path(data_dir, slot)
}

fn plant_orphan_slot(data_dir: &Path, slot: usize, pgid: u32, lstart: &str) {
    let lock = slot_path(data_dir, slot);
    let token = token_path(data_dir, slot);
    fs::create_dir_all(lock.parent().unwrap()).unwrap();
    fs::create_dir_all(token.parent().unwrap()).unwrap();
    fs::write(&token, format!("token={TOKEN}\n")).unwrap();
    fs::write(
        &lock,
        format!("reserved=unknown\npgid={pgid}\nlstart={lstart}\n"),
    )
    .unwrap();
}

#[cfg(unix)]
fn inode_of(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).unwrap().ino()
}

fn run_short_admit(
    data_dir: &Path,
    clock: &Arc<FakeClock>,
    control: &ScriptedControl,
    heavy_wait_secs: u64,
    budget_secs: u64,
    label: &str,
) -> Option<usize> {
    let machine = MachineConfig {
        max_heavy_parallel: 1,
        heavy_wait_secs,
        ..MachineConfig::default()
    };
    let sleeper = AdvancingSleeper {
        clock: Arc::clone(clock),
    };
    let throttle = Throttle {
        machine: &machine,
        probe: &CalmProbe,
        sleeper: &sleeper,
        control,
        progress: &|_| {},
        cpus: 4,
        tools: PriorityTools::default(),
        jitter_seed: 7,
        sample_every: SAMPLE_EVERY,
    };
    let ask = Ask {
        repo_id: "repo-rfc046",
        label,
        tool: None,
        cmd_family: None,
        exclusive: false,
        wait_limit_secs: Some(budget_secs),
    };
    throttle
        .admit_ask(data_dir, &**clock, &ask)
        .unwrap()
        .map(|admitted| admitted.slot.index())
}

fn verify_cross_call_reap(measured: bool) {
    let data = TempDir::new().unwrap();
    plant_orphan_slot(data.path(), 0, PGID_A, LSTART_A);

    let lock = slot_path(data.path(), 0);
    #[cfg(unix)]
    let ino_before = inode_of(&lock);

    let clock = Arc::new(FakeClock(AtomicI64::new(10_000)));
    if measured {
        let sleeper = AdvancingSleeper {
            clock: Arc::clone(&clock),
        };
        let sample = HeavySample {
            peak_rss_bytes: 256 * 1024 * 1024,
            duration_ms: 100,
        };
        assert!(record(data.path(), "repo-rfc046", "test_full", sample, &sleeper).unwrap());
        clock.0.store(10_000, Ordering::SeqCst);
    }

    let control = ScriptedControl::live(PGID_A, LSTART_A);

    // Call 1: 10s budget < 20s heavy_wait_secs -> records persisted orphaned_since, does NOT reap.
    let first = run_short_admit(data.path(), &clock, &control, 20, 10, "test_full");
    assert_eq!(
        first, None,
        "call 1 must not reap before 20s heavy_wait_secs elapses"
    );

    let sidecar = sidecar_path(data.path(), 0);
    assert!(
        sidecar.exists(),
        "RFC-046: persisted sidecar must be created at {}",
        sidecar.display()
    );
    let sidecar_text = fs::read_to_string(&sidecar).unwrap();
    assert!(
        sidecar_text.contains(&format!("pgid={PGID_A}"))
            && sidecar_text.contains(&format!("lstart={LSTART_A}"))
            && sidecar_text.contains("since=10000"),
        "unexpected sidecar contents: {sidecar_text:?}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&sidecar).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "sidecar must be owner-only 0600");
    }

    // Call 2: another 10s budget (< 20s on its own), continuing clock across the 20s mark.
    // Because orphaned_since=10000 is persisted in locks/, call 2 must reap and admit slot 0.
    let second = run_short_admit(data.path(), &clock, &control, 20, 10, "test_full");
    assert_eq!(
        second,
        Some(0),
        "RFC-046: call 2 must reap orphan once total clock age from persisted orphaned_since >= 20s"
    );
    assert!(
        !sidecar.exists(),
        "RFC-046: sidecar must be removed once orphan is reaped/claimed"
    );
    #[cfg(unix)]
    {
        assert_eq!(
            inode_of(&lock),
            ino_before,
            "RFC-046: heavy.0.lock must NEVER be unlinked during reap"
        );
    }
}

#[test]
fn rfc046_persisted_orphaned_since_reaps_across_short_calls_unmeasured() {
    verify_cross_call_reap(false);
}

#[test]
fn rfc046_persisted_orphaned_since_reaps_across_short_calls_measured() {
    verify_cross_call_reap(true);
}

#[test]
fn rfc046_identity_change_resets_orphaned_since_for_pgid_and_lstart() {
    let data = TempDir::new().unwrap();
    plant_orphan_slot(data.path(), 0, PGID_A, LSTART_A);

    let clock = Arc::new(FakeClock(AtomicI64::new(10_000)));
    let control = ScriptedControl::live(PGID_A, LSTART_A);

    // Call 1 records orphaned_since=10000 for (PGID_A, LSTART_A).
    assert_eq!(
        run_short_admit(data.path(), &clock, &control, 20, 10, "build"),
        None
    );

    // Advance clock past 10_000 + 20, then swap to a different PGID (PGID_B, LSTART_A).
    clock.0.store(10_025, Ordering::SeqCst);
    plant_orphan_slot(data.path(), 0, PGID_B, LSTART_A);
    control.set_live(PGID_B, LSTART_A);

    // Short 5s wait must NOT reap PGID_B using PGID_A's old timestamp!
    assert_eq!(
        run_short_admit(data.path(), &clock, &control, 20, 5, "build"),
        None,
        "RFC-046: changing pgid must reset orphaned_since instead of inheriting prior orphan age"
    );
    let sidecar_text = fs::read_to_string(sidecar_path(data.path(), 0)).unwrap();
    assert!(
        sidecar_text.contains(&format!("pgid={PGID_B}")) && sidecar_text.contains("since=10025"),
        "expected reset sidecar for PGID_B at 10025, got {sidecar_text:?}"
    );

    // Advance clock past 10_025 + 20, then swap to same PGID_B with DIFFERENT lstart (LSTART_B).
    clock.0.store(10_050, Ordering::SeqCst);
    plant_orphan_slot(data.path(), 0, PGID_B, LSTART_B);
    control.set_live(PGID_B, LSTART_B);

    // Short 5s wait must NOT reap (PGID_B, LSTART_B) using LSTART_A's old timestamp!
    assert_eq!(
        run_short_admit(data.path(), &clock, &control, 20, 5, "build"),
        None,
        "RFC-046: changing lstart must reset orphaned_since instead of inheriting prior orphan age"
    );
    let sidecar_text2 = fs::read_to_string(sidecar_path(data.path(), 0)).unwrap();
    assert!(
        sidecar_text2.contains(&format!("pgid={PGID_B}"))
            && sidecar_text2.contains(&format!("lstart={LSTART_B}"))
            && sidecar_text2.contains("since=10050"),
        "expected reset sidecar for LSTART_B at 10050, got {sidecar_text2:?}"
    );
}

#[test]
fn rfc046_unreadable_group_state_preserves_lock_and_sidecar() {
    let data = TempDir::new().unwrap();
    plant_orphan_slot(data.path(), 0, PGID_A, LSTART_A);
    let expected_lock = fs::read_to_string(slot_path(data.path(), 0)).unwrap();

    let sidecar = sidecar_path(data.path(), 0);
    let expected_sidecar = format!("pgid={PGID_A}\nlstart={LSTART_A}\nsince=10000\n");
    fs::write(&sidecar, &expected_sidecar).unwrap();

    let clock = Arc::new(FakeClock(AtomicI64::new(10_500)));
    let control = ScriptedControl::unreadable(PGID_A);

    assert_eq!(
        run_short_admit(data.path(), &clock, &control, 20, 3, "build"),
        None
    );
    assert_eq!(
        fs::read_to_string(slot_path(data.path(), 0)).unwrap(),
        expected_lock,
        "lock file must stay untouched on Err(Unreadable)"
    );
    assert_eq!(
        fs::read_to_string(&sidecar).unwrap(),
        expected_sidecar,
        "sidecar must stay untouched on Err(Unreadable)"
    );
}

#[test]
fn rfc046_claim_and_release_clean_sidecar() {
    let data = TempDir::new().unwrap();
    plant_orphan_slot(data.path(), 0, PGID_A, LSTART_A);
    let sidecar = sidecar_path(data.path(), 0);
    fs::write(
        &sidecar,
        format!("pgid={PGID_A}\nlstart={LSTART_A}\nsince=10000\n"),
    )
    .unwrap();

    // Dead orphan -> try_acquire_in claims slot 0 and cleans sidecar.
    let view = MachineView {
        free_memory: &|| Some(u64::MAX),
        group_rss: &|_| None,
        group_state: &|_| None,
    };
    let guard = try_acquire_in(data.path(), 1, Admission::Unmeasured, &view)
        .unwrap()
        .unwrap();
    assert!(
        !sidecar.exists(),
        "claiming a slot must remove any stale orphan_since sidecar"
    );

    // Simulate a leftover sidecar before release -> dropping SlotGuard must remove it too.
    fs::write(
        &sidecar,
        format!("pgid={PGID_A}\nlstart={LSTART_A}\nsince=10000\n"),
    )
    .unwrap();
    drop(guard);
    assert!(
        !sidecar.exists(),
        "releasing a slot must remove any orphan_since sidecar"
    );
}

#[test]
fn rfc046_heavy_wait_secs_zero_never_reaps_live_orphan() {
    let data = TempDir::new().unwrap();
    plant_orphan_slot(data.path(), 0, PGID_A, LSTART_A);

    let clock = Arc::new(FakeClock(AtomicI64::new(10_000)));
    let control = ScriptedControl::live(PGID_A, LSTART_A);

    assert_eq!(
        run_short_admit(data.path(), &clock, &control, 0, 5, "build"),
        None,
        "heavy_wait_secs == 0 must not reap live orphan"
    );
    let lock_after = fs::read_to_string(slot_path(data.path(), 0)).unwrap();
    assert!(
        lock_after.contains(&format!("pgid={PGID_A}")),
        "live orphan lock record must remain intact when heavy_wait_secs == 0"
    );
}

#[test]
fn rfc046_busy_orphaned_display_includes_ps_inspect_hint() {
    let rendered = format!("{}", Busy::Orphaned { pgid: PGID_A });
    assert!(
        rendered.contains(&format!("ps -o pid,lstart,command -g {PGID_A}")),
        "Busy::Orphaned Display must include ps inspection hint, got: {rendered:?}"
    );
}

struct TwoSlotControl {
    slot0_unreadable: bool,
}

impl GroupControl for TwoSlotControl {
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
        if pgid == PGID_A {
            if self.slot0_unreadable {
                Err(Unreadable)
            } else {
                Ok(Some(GroupState {
                    stopped: false,
                    leader_started: Some(LSTART_A.to_string()),
                }))
            }
        } else if pgid == PGID_B {
            Ok(Some(GroupState {
                stopped: false,
                leader_started: Some(LSTART_B.to_string()),
            }))
        } else {
            Ok(None)
        }
    }
}

fn run_two_slot_measured_admit(
    data: &Path,
    now: i64,
    wait_limit_secs: u64,
    control: &dyn GroupControl,
) -> Option<usize> {
    let clock = Arc::new(FakeClock(AtomicI64::new(now)));
    let sleeper = AdvancingSleeper {
        clock: Arc::clone(&clock),
    };
    let sample = HeavySample {
        peak_rss_bytes: 64 * 1024 * 1024,
        duration_ms: 100,
    };
    assert!(record(data, "repo-rfc046", "test_scoped", sample, &sleeper).unwrap());
    clock.0.store(now, Ordering::SeqCst);

    let machine = MachineConfig {
        max_heavy_parallel: 2,
        heavy_wait_secs: 20,
        ..MachineConfig::default()
    };
    let throttle = Throttle {
        machine: &machine,
        probe: &CalmProbe,
        sleeper: &sleeper,
        control,
        progress: &|_| {},
        cpus: 4,
        tools: PriorityTools::default(),
        jitter_seed: 7,
        sample_every: SAMPLE_EVERY,
    };
    let ask = Ask {
        repo_id: "repo-rfc046",
        label: "test_scoped",
        tool: None,
        cmd_family: None,
        exclusive: false,
        wait_limit_secs: Some(wait_limit_secs),
    };
    throttle
        .admit_ask(data, &*clock, &ask)
        .unwrap()
        .map(|a| a.slot.index())
}

#[test]
fn rfc046_multi_slot_reap_never_clobbers_unreadable_sibling_slot() {
    let data = TempDir::new().unwrap();
    // Slot 0 is unreadable (ps timeout); slot 1 is a live orphan.
    plant_orphan_slot(data.path(), 0, PGID_A, LSTART_A);
    let expected_lock0 = fs::read_to_string(slot_path(data.path(), 0)).unwrap();
    let sidecar0 = sidecar_path(data.path(), 0);
    let expected_sidecar0 = format!("pgid={PGID_A}\nlstart={LSTART_A}\nsince=10000\n");
    fs::write(&sidecar0, &expected_sidecar0).unwrap();

    plant_orphan_slot(data.path(), 1, PGID_B, LSTART_B);
    let sidecar1 = sidecar_path(data.path(), 1);
    fs::write(
        &sidecar1,
        format!("pgid={PGID_B}\nlstart={LSTART_B}\nsince=10000\n"),
    )
    .unwrap();

    let control = TwoSlotControl {
        slot0_unreadable: true,
    };
    // Slot 1 gets reaped (timed out), slot 0 stays busy (`reserved=unknown`) and untouched.
    let _ = run_two_slot_measured_admit(data.path(), 10_025, 3, &control);

    assert_eq!(
        fs::read_to_string(slot_path(data.path(), 0)).unwrap(),
        expected_lock0,
        "unreadable slot 0 lock must remain intact when sibling slot 1 is reaped"
    );
    assert_eq!(
        fs::read_to_string(&sidecar0).unwrap(),
        expected_sidecar0,
        "unreadable slot 0 sidecar must remain intact when sibling slot 1 is reaped"
    );
    assert_eq!(
        fs::read_to_string(slot_path(data.path(), 1)).unwrap(),
        "",
        "timed-out orphan slot 1 must be truncated via set_len(0)"
    );
    assert!(
        !sidecar1.exists(),
        "timed-out orphan slot 1 sidecar must be removed after reap"
    );
}

#[test]
fn rfc046_live_flock_slot_never_gets_orphan_since_sidecar_during_admit() {
    let data = TempDir::new().unwrap();
    let view = MachineView {
        free_memory: &|| Some(u64::MAX),
        group_rss: &|_| None,
        group_state: &|_| None,
    };
    let mut held = try_acquire_in(
        data.path(),
        2,
        Admission::Measured {
            peak_rss_bytes: 64 * 1024 * 1024,
        },
        &view,
    )
    .unwrap()
    .unwrap();
    assert_eq!(held.index(), 0);
    held.record_group(PGID_A, Some(LSTART_A.to_string()));
    plant_orphan_slot(data.path(), 1, PGID_B, LSTART_B);

    let control = TwoSlotControl {
        slot0_unreadable: false,
    };
    assert_eq!(
        run_two_slot_measured_admit(data.path(), 10_000, 5, &control),
        None
    );
    assert!(
        !sidecar_path(data.path(), 0).exists(),
        "live flock-held slot 0 must NEVER have heavy.0.orphan_since created"
    );
    assert!(
        sidecar_path(data.path(), 1).exists(),
        "unlocked orphan slot 1 should have been observed while slot 0 was skipped"
    );
    assert!(
        fs::read_to_string(slot_path(data.path(), 0))
            .unwrap()
            .contains(&format!("pgid={PGID_A}"))
    );
    drop(held);
}
