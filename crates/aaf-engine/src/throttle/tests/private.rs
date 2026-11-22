//! The throttle's own files (slot locks, drain lock, pause records) and the
//! directories it creates are owner-only, also when the gate never ran.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use tempfile::TempDir;

use super::super::orphans::record_pause;
use super::super::slots::{Admission, try_acquire, try_drain};

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn slot_drain_and_pause_files_are_created_private_in_a_private_data_dir() {
    let tmp = TempDir::new().unwrap();
    // Not there yet: the first `aaf slot` on a machine creates it.
    let data = tmp.path().join("autofactory");
    let _slot = try_acquire(&data, 1, Admission::Unmeasured, &|| None)
        .unwrap()
        .unwrap();
    let _drain = try_drain(&data).unwrap().unwrap();
    let _pause = record_pause(&data, 4242, Some("Sun Nov 22 11:00:00 2026")).unwrap();

    assert_eq!(mode(&data), 0o700);
    for dir in ["throttle", "throttle/guest", "locks"] {
        assert_eq!(mode(&data.join(dir)), 0o700, "{dir}");
    }
    for name in [
        "throttle/heavy.0.lock",
        "throttle/drain.lock",
        "locks/heavy.0.token",
        "locks/paused.4242",
    ] {
        assert_eq!(mode(&data.join(name)), 0o600, "{name}");
    }
}

#[test]
fn an_existing_open_data_dir_is_tightened_by_the_first_slot() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("autofactory");
    fs::create_dir_all(data.join("throttle")).unwrap();
    for dir in [data.clone(), data.join("throttle")] {
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fs::write(data.join("throttle/drain.lock"), "").unwrap();
    fs::set_permissions(
        data.join("throttle/drain.lock"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();

    let _slot = try_acquire(&data, 1, Admission::Unmeasured, &|| None)
        .unwrap()
        .unwrap();
    let _drain = try_drain(&data).unwrap().unwrap();
    assert_eq!(mode(&data), 0o700);
    assert_eq!(mode(&data.join("throttle")), 0o700);
    assert_eq!(mode(&data.join("throttle/drain.lock")), 0o600);
}
