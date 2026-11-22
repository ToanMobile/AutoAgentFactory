//! The gate under machine load control (docs/PLAN.md C.11): light stages
//! run at once, build and test commands (and a lint that starts a build
//! tool) queue for a heavy slot, and
//! waiting is recorded without changing the status.

use std::sync::{Arc, Mutex};

use aaf_domain::config::MachineConfig;
use aaf_domain::gate::{GateMode, GateStatus};
use aaf_domain::ports::Clock;

use super::super::runner::is_heavy;
use super::super::{run_gate_in, run_gate_throttled, run_gate_with, stats_repo_id};
use super::support::{FixedClock, Repo, fake_secret, passing_cfg};
use crate::throttle::slots::{Admission, try_acquire};
use crate::throttle::stats::recorded_peak;
use crate::throttle::tests::support::{MarksSampled, Rig, after_file};
use crate::throttle::{BuildDaemon, Pressure, SlotGuard, Throttle};

/// A machine with a single heavy slot, taken by "another process".
fn busy_machine(repo: &Repo) -> (Rig, SlotGuard) {
    let mut rig = Rig::calm();
    rig.machine.max_heavy_parallel = 1;
    let measured = Admission::Measured { peak_rss_bytes: 1 };
    let held = try_acquire(repo.data_dir(), 1, measured, &|| None)
        .unwrap()
        .unwrap();
    (rig, held)
}

fn never_sleeps(rig: &Rig) {
    rig.sleeper
        .on_sleep(|_| panic!("a light stage waited for the machine"));
}

#[test]
fn build_and_test_commands_are_heavy_and_lint_only_when_it_starts_a_build_tool() {
    for name in ["build", "test_scoped", "test_full"] {
        assert!(is_heavy(name, "true"), "{name}");
    }
    assert!(!is_heavy("lint", "cargo fmt --check"));
    assert!(is_heavy("lint", "./gradlew lint"));
    assert!(is_heavy("lint", "cd app && ./gradlew ktlintCheck detekt"));
    assert!(is_heavy("lint", "xcodebuild analyze"));
}

/// The default Android lint is `./gradlew lint`: as heavy as a build, and it
/// starts a daemon. It must queue and get the daemon switches like one.
#[test]
fn lint_that_starts_gradle_waits_for_the_slot_and_gets_the_daemon_switches() {
    let repo = Repo::new();
    repo.write(".gitignore", "*.mark\n");
    repo.write("gradlew", "printf '%s' \"$GRADLE_OPTS\" > lint.mark\n");
    repo.commit("wrapper");
    let (rig, held) = busy_machine(&repo);
    let held = Arc::new(Mutex::new(Some(held)));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let lint_mark = repo.path("lint.mark");
    let (release, log) = (Arc::clone(&held), Arc::clone(&seen));
    rig.sleeper.on_sleep(move |count| {
        log.lock().unwrap().push(lint_mark.exists());
        if count == 2 {
            release.lock().unwrap().take();
        }
    });
    let mut cfg = passing_cfg();
    cfg.commands.lint = "sh ./gradlew lint".to_string();
    cfg.commands.test_scoped = "true".to_string();

    let receipt = repo.gate_on(&rig, &cfg, GateMode::Scoped, &*rig.clock);
    assert_eq!(receipt.status, GateStatus::Clear, "{:?}", receipt.reasons);
    // While the slot was taken the lint had not started.
    assert_eq!(*seen.lock().unwrap(), vec![false; 2]);
    let opts = std::fs::read_to_string(repo.path("lint.mark")).unwrap();
    assert!(opts.contains("-Dorg.gradle.daemon=false"), "{opts}");
    assert!(
        receipt
            .reasons
            .iter()
            .any(|reason| reason.starts_with("throttle: lint waited")),
        "{:?}",
        receipt.reasons
    );
}

#[test]
fn failing_lint_blocks_without_waiting_for_a_slot() {
    let repo = Repo::new();
    let (rig, _held) = busy_machine(&repo);
    never_sleeps(&rig);
    let mut cfg = passing_cfg();
    cfg.commands.lint = "echo style >&2; exit 1".to_string();
    cfg.commands.test_scoped = "touch tests-ran".to_string();

    let receipt = repo.gate_on(&rig, &cfg, GateMode::Scoped, &*rig.clock);
    assert_eq!(receipt.status, GateStatus::Blocked);
    assert_eq!(receipt.reasons, vec!["command lint failed (exit 1)"]);
    assert!(!repo.path("tests-ran").exists());
}

#[test]
fn static_finding_blocks_without_waiting_for_a_slot() {
    let repo = Repo::new();
    let (rig, _held) = busy_machine(&repo);
    never_sleeps(&rig);
    repo.write(
        "src/k.rs",
        &format!("const K: &str = \"{}\";\n", fake_secret()),
    );

    let receipt = repo.gate_on(&rig, &passing_cfg(), GateMode::Scoped, &*rig.clock);
    assert_eq!(receipt.status, GateStatus::Blocked);
    assert!(receipt.commands.is_empty());
}

#[test]
fn heavy_command_waits_for_the_slot_after_lint_and_the_wait_is_recorded() {
    let repo = Repo::new();
    repo.write(".gitignore", "*.mark\n");
    repo.commit("ignore markers");
    let (rig, held) = busy_machine(&repo);
    let held = Arc::new(Mutex::new(Some(held)));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (lint_mark, test_mark) = (repo.path("lint.mark"), repo.path("test.mark"));
    let (release, log) = (Arc::clone(&held), Arc::clone(&seen));
    rig.sleeper.on_sleep(move |count| {
        // While the gate waits, lint has already run and the test has not.
        log.lock()
            .unwrap()
            .push((lint_mark.exists(), test_mark.exists()));
        if count == 3 {
            release.lock().unwrap().take();
        }
    });
    let mut cfg = passing_cfg();
    cfg.commands.lint = "touch lint.mark".to_string();
    cfg.commands.test_scoped = "touch test.mark".to_string();

    let before = rig.clock.now();
    let receipt = repo.gate_on(&rig, &cfg, GateMode::Scoped, &*rig.clock);
    let waited = rig.clock.now() - before;
    assert_eq!(*seen.lock().unwrap(), vec![(true, false); 3]);
    assert!(repo.path("test.mark").exists());
    // Waiting is not a failure: the status is what the commands decided.
    assert_eq!(receipt.status, GateStatus::Clear);
    assert_eq!(
        receipt.reasons,
        vec![format!(
            "throttle: test_scoped waited {waited}s for a heavy slot or a calmer machine"
        )]
    );
    assert!(waited >= 3);
    assert!(rig.events_starting("WaitingForSlot") >= 1);
}

#[test]
fn free_slot_and_calm_machine_leave_no_note() {
    let repo = Repo::new();
    let rig = Rig::calm();
    never_sleeps(&rig);
    let mut cfg = passing_cfg();
    cfg.commands.build = "true".to_string();
    cfg.commands.test_full = "true".to_string();
    let receipt = repo.gate_on(&rig, &cfg, GateMode::Full, &*rig.clock);
    assert_eq!(receipt.status, GateStatus::Clear);
    assert!(receipt.reasons.is_empty(), "{:?}", receipt.reasons);
    assert_eq!(receipt.commands.len(), 3);
}

#[test]
fn load_that_never_drops_runs_degraded_and_stays_clear() {
    let repo = Repo::new();
    let mut rig = Rig::calm();
    rig.machine.heavy_wait_secs = 30;
    rig.probe.load.set(vec![Some(400)]);
    let before = rig.clock.now();
    let receipt = repo.gate_on(&rig, &passing_cfg(), GateMode::Scoped, &*rig.clock);
    let waited = rig.clock.now() - before;
    assert!(waited >= 30);
    assert_eq!(receipt.status, GateStatus::Clear);
    assert_eq!(
        receipt.reasons,
        vec![
            format!("throttle: test_scoped waited {waited}s for a heavy slot or a calmer machine"),
            "throttle: test_scoped ran degraded: CPU load stayed above 150% for 30s".to_string(),
        ]
    );
}

#[test]
fn memory_pressure_delays_the_test_but_not_a_failing_result() {
    let repo = Repo::new();
    let rig = Rig::calm();
    rig.probe.pressure.set(vec![
        Some(Pressure::Critical),
        Some(Pressure::Warn),
        Some(Pressure::Normal),
    ]);
    let mut cfg = passing_cfg();
    cfg.commands.test_scoped = "exit 4".to_string();
    let receipt = repo.gate_on(&rig, &cfg, GateMode::Scoped, &*rig.clock);
    assert_eq!(rig.sleeper.count(), 2);
    // The failure keeps its status and its reason comes first.
    assert_eq!(receipt.status, GateStatus::Blocked);
    assert_eq!(receipt.reasons[0], "command test_scoped failed (exit 4)");
    assert!(receipt.reasons[1].starts_with("throttle: test_scoped waited"));
}

#[test]
fn worktrees_of_one_repository_share_their_memory_statistics() {
    let repo = Repo::new();
    let other = tempfile::TempDir::new().unwrap();
    let path = other.path().join("wt");
    repo.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "task",
        path.to_str().unwrap(),
    ]);
    assert_eq!(stats_repo_id(&path), stats_repo_id(repo.root()));
    // The receipt id stays per checkout; only the statistics are shared.
    assert_ne!(aaf_fp::repo_id(&path), aaf_fp::repo_id(repo.root()));

    let unrelated = Repo::new();
    assert_ne!(stats_repo_id(unrelated.root()), stats_repo_id(repo.root()));
    // Not a repository at all: falls back to the directory's own id.
    assert_eq!(stats_repo_id(other.path()), aaf_fp::repo_id(other.path()));
}

#[test]
fn gate_records_the_heavy_command_under_the_shared_statistics_key() {
    let repo = Repo::new();
    let rig = Rig::calm();
    // The command waits until it has been sampled: under load a `sleep 0.5`
    // had ended before the first sample and nothing was recorded. The
    // marker lives outside the repository, whose tree the gate reads.
    let outside = tempfile::TempDir::new().unwrap();
    let control = MarksSampled {
        file: outside.path().join("sampled"),
    };
    let mut cfg = passing_cfg();
    cfg.commands.test_scoped = after_file(&control.file, "exit 0");
    let throttle = Throttle {
        control: &control,
        ..rig.throttle()
    };
    let receipt = run_gate_throttled(
        repo.data_dir(),
        repo.root(),
        &cfg,
        GateMode::Scoped,
        &FixedClock(1),
        &throttle,
    )
    .unwrap();
    assert_eq!(receipt.status, GateStatus::Clear);
    let id = stats_repo_id(&std::fs::canonicalize(repo.root()).unwrap());
    assert!(recorded_peak(repo.data_dir(), &id, "test_scoped").is_some());
    // Lint is light: it never goes through the throttle, so it has no entry.
    assert_eq!(recorded_peak(repo.data_dir(), &id, "lint"), None);
}

#[test]
fn gradle_daemon_left_running_is_a_reason_and_the_gate_stays_clear() {
    use std::os::unix::fs::PermissionsExt;

    let repo = Repo::new();
    // A stand-in that does nothing: no real Gradle is started.
    repo.write("gradlew", "#!/bin/sh\nexit 0\n");
    let mode = std::fs::Permissions::from_mode(0o755);
    std::fs::set_permissions(repo.path("gradlew"), mode).unwrap();
    repo.commit("wrapper");
    let rig = Rig::calm();
    let left = BuildDaemon {
        pid: 4321,
        kind: "gradle",
        started: "Sun Nov 22 09:12:08 2026".to_string(),
        rss_bytes: 1,
    };
    // Nothing before the command, one daemon after it.
    *rig.group.daemons.lock().unwrap() = vec![Vec::new(), vec![left]].into();
    let mut cfg = passing_cfg();
    cfg.commands.test_scoped = "./gradlew test".to_string();

    let receipt = repo.gate_on(&rig, &cfg, GateMode::Scoped, &*rig.clock);
    assert_eq!(receipt.status, GateStatus::Clear);
    assert_eq!(
        receipt.reasons,
        vec!["throttle: gradle daemon still running pid=4321; builds are not using --no-daemon"]
    );
}

/// The real entry points on the real machine. Blocked before any command,
/// so nothing here depends on the load of the machine running the tests.
#[test]
fn public_entry_points_run_on_the_real_machine() {
    let repo = Repo::new();
    repo.write(
        "src/k.rs",
        &format!("const K: &str = \"{}\";\n", fake_secret()),
    );
    let clock = FixedClock(9);
    let machine = MachineConfig {
        max_heavy_parallel: 1,
        ..MachineConfig::default()
    };
    let with = run_gate_with(
        repo.data_dir(),
        repo.root(),
        &passing_cfg(),
        GateMode::Scoped,
        &clock,
        &machine,
    )
    .unwrap();
    assert_eq!(with.status, GateStatus::Blocked);
    let plain = run_gate_in(
        repo.data_dir(),
        repo.root(),
        &passing_cfg(),
        GateMode::Scoped,
        &clock,
    )
    .unwrap();
    assert_eq!(plain, with);
}
