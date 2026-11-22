#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]

//! The store holds task specs, prompts' notes and account paths: its files
//! are for the owner only, whatever the umask says.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use aaf_domain::ports::KvStore;
use aaf_engine::db::{Db, private};

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn a_new_database_and_its_wal_files_are_owner_only() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("aaf.db");

    let db = Db::open(&path).unwrap();
    db.set_meta("k", "v").unwrap();

    assert_eq!(mode(&path), 0o600);
    for suffix in ["-wal", "-shm"] {
        let side = tmp.path().join(format!("aaf.db{suffix}"));
        assert!(side.exists(), "{suffix} file expected while the db is open");
        assert_eq!(mode(&side), 0o600, "{suffix}");
    }
}

#[test]
fn an_existing_world_readable_database_is_tightened_and_keeps_its_data() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("aaf.db");
    {
        let db = Db::open(&path).unwrap();
        db.set_meta("k", "kept").unwrap();
    }
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

    let db = Db::open(&path).unwrap();

    assert_eq!(mode(&path), 0o600);
    assert_eq!(db.get_meta("k").unwrap().as_deref(), Some("kept"));
}

#[test]
fn private_dirs_and_files_ignore_the_umask_and_tighten_what_exists() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data/leases");

    private::create_dir(&dir).unwrap();
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&tmp.path().join("data")), 0o700);

    let file = dir.join("a.lock");
    drop(private::open_file(&file).unwrap());
    assert_eq!(mode(&file), 0o600);

    // Left behind by an earlier version.
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    fs::write(&file, "kept").unwrap();
    private::create_dir(&dir).unwrap();
    drop(private::open_file(&file).unwrap());
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&file), 0o600);
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "kept",
        "never truncated"
    );
}
