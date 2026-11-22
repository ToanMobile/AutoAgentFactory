//! What the gate leaves in the data dir is readable by its owner only: the
//! directory, the receipt, the slot locks and the learned statistics.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use aaf_domain::gate::{GateMode, GateStatus};

use super::support::{Repo, passing_cfg};
use crate::throttle::tests::support::Rig;

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// Every entry below `dir`, with its mode.
fn modes_below(dir: &Path, out: &mut Vec<(String, u32, bool)>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let is_dir = path.is_dir();
        out.push((path.display().to_string(), mode(&path), is_dir));
        if is_dir {
            modes_below(&path, out);
        }
    }
}

#[test]
fn gate_makes_an_open_data_dir_private_and_writes_owner_only_files() {
    let repo = Repo::new();
    // As an earlier version left it: open directory, world-readable files.
    fs::set_permissions(repo.data_dir(), fs::Permissions::from_mode(0o755)).unwrap();
    fs::create_dir(repo.data_dir().join("receipts")).unwrap();
    fs::set_permissions(
        repo.data_dir().join("receipts"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let rig = Rig::calm();
    // A memory reading, so that the run is remembered in heavy_stats.json.
    rig.group.rss.lock().unwrap().push_back(Some(4096));
    let receipt = repo.gate_on(&rig, &passing_cfg(), GateMode::Scoped, &*rig.clock);
    assert_eq!(receipt.status, GateStatus::Clear, "{:?}", receipt.reasons);

    assert_eq!(mode(repo.data_dir()), 0o700);
    let mut entries = Vec::new();
    modes_below(repo.data_dir(), &mut entries);
    // The receipt, the statistics and the locks are all there.
    for name in ["receipts", "latest.json", "locks", "heavy_stats.json"] {
        assert!(
            entries.iter().any(|(path, ..)| path.ends_with(name)),
            "{name} missing: {entries:#?}"
        );
    }
    for (path, mode, is_dir) in &entries {
        let expected = if *is_dir { 0o700 } else { 0o600 };
        assert_eq!(*mode, expected, "{path}");
    }
}
