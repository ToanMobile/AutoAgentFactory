//! gc must never probe (shared-flock) a fresh orphan merge-scratch lock: its
//! owner may be about to `try_lock` it, and a probe in that window can make
//! the owner hit a lock timeout. Age is checked before the probe.

use std::cell::Cell;
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use super::orphan_scratch_lock_removable;

const MAX_AGE: Duration = Duration::from_secs(14 * 86_400);

fn orphan_lock(dir: &Path) -> (std::path::PathBuf, fs::Metadata) {
    let path = dir.join("t1-pr7-4242-1.lock");
    fs::write(&path, b"").unwrap();
    let meta = fs::symlink_metadata(&path).unwrap();
    (path, meta)
}

#[test]
fn a_fresh_orphan_lock_is_left_alone_and_never_probed() {
    let tmp = tempfile::tempdir().unwrap();
    let (path, meta) = orphan_lock(tmp.path());
    let probes = Cell::new(0u32);
    let removable = orphan_scratch_lock_removable(&path, &meta, MAX_AGE, SystemTime::now(), |_| {
        probes.set(probes.get() + 1);
        false
    });
    assert!(!removable, "a fresh orphan lock must be kept");
    assert_eq!(
        probes.get(),
        0,
        "gc took a shared flock on a fresh orphan lock its owner may be about to try_lock"
    );
}

#[test]
fn an_old_orphan_lock_is_probed_once_and_removed_only_when_unheld() {
    let tmp = tempfile::tempdir().unwrap();
    let (path, meta) = orphan_lock(tmp.path());
    let later = SystemTime::now() + MAX_AGE + Duration::from_secs(60);

    let probes = Cell::new(0u32);
    let probe = |held: bool| {
        let probes = &probes;
        move |_: &Path| {
            probes.set(probes.get() + 1);
            held
        }
    };
    assert!(orphan_scratch_lock_removable(
        &path,
        &meta,
        MAX_AGE,
        later,
        probe(false)
    ));
    assert!(!orphan_scratch_lock_removable(
        &path,
        &meta,
        MAX_AGE,
        later,
        probe(true)
    ));
    assert_eq!(probes.get(), 2);

    // Not an orphan: its scratch dir still exists.
    fs::create_dir(path.with_extension("")).unwrap();
    assert!(!orphan_scratch_lock_removable(
        &path,
        &meta,
        MAX_AGE,
        later,
        probe(false)
    ));
}
