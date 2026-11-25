#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Step 1 & Round-1 (Findings #1, #2) tests: `same_cow_device`, mount parsing,
//! fail-closed `dev()` match, nested mount detection, and nearest-ancestor errors.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use aaf_engine::git::hydration::device::{
    check_no_nested_mount_with, find_mount_for_path, is_apfs_mount_with_dev,
    nearest_existing_canonical, parse_macos_mount_output, same_cow_device,
    same_cow_device_from_meta, same_cow_device_with_mounts,
};

const SAMPLE_MOUNT_OUTPUT: &str = "\
/dev/disk3s8s1 on / (apfs, sealed, local, read-only, journaled)
devfs on /dev (devfs, local, nobrowse)
/dev/disk3s1 on /System/Volumes/Data (apfs, local, journaled, nobrowse, protect, root data)
/dev/disk3s7 on /Volumes/Data (apfs, local, journaled, protect)
/dev/disk6s1 on /Volumes/Data2 (hfs, local, nodev, nosuid, journaled)
/dev/disk8s1 on /Volumes/My Ext Drive (exfat, local, nodev, nosuid)
/dev/disk9s1 on /Volumes/Grok Bot Installer (apfs, local, nodev, nosuid, read-only, journaled)
";

#[test]
fn same_device_different_dev_returns_false() {
    assert!(!same_cow_device_from_meta(100, 200, Some(true)));
    assert!(!same_cow_device_from_meta(100, 200, None));
    assert!(same_cow_device_from_meta(100, 100, Some(true)));
    assert!(same_cow_device_from_meta(100, 100, None));
}

#[test]
fn same_device_same_dev_non_apfs_mount_returns_false() {
    assert!(!same_cow_device_from_meta(42, 42, Some(false)));

    let tmp = tempfile::tempdir().unwrap();
    let canon = fs::canonicalize(tmp.path()).unwrap();
    let src = canon.join("src");
    let dst_parent = canon.join("dst/nested");
    fs::create_dir_all(&src).unwrap();

    let non_apfs_mounts = parse_macos_mount_output(&format!(
        "/dev/disk9s1 on {} (hfs, local, journaled)\n",
        canon.display()
    ));
    assert!(
        !same_cow_device_with_mounts(&src, &dst_parent, Some(&non_apfs_mounts)).unwrap(),
        "same dev on non-APFS mount must return false"
    );

    let apfs_mounts = parse_macos_mount_output(&format!(
        "/dev/disk9s1 on {} (apfs, local, journaled)\n",
        canon.display()
    ));
    assert!(
        same_cow_device_with_mounts(&src, &dst_parent, Some(&apfs_mounts)).unwrap(),
        "same dev on APFS mount must return true"
    );
}

#[test]
fn same_device_parse_mount_matches_by_component_not_prefix() {
    let mounts = parse_macos_mount_output(SAMPLE_MOUNT_OUTPUT);
    assert_eq!(mounts.len(), 7, "parsed mounts: {mounts:?}");

    let m_data2 = find_mount_for_path(Path::new("/Volumes/Data2/repo"), &mounts).unwrap();
    assert_eq!(m_data2.mount_point, Path::new("/Volumes/Data2"));
    assert_eq!(m_data2.fs_type, "hfs");
    assert!(!m_data2.read_only);

    let m_data = find_mount_for_path(Path::new("/Volumes/Data/proj"), &mounts).unwrap();
    assert_eq!(m_data.mount_point, Path::new("/Volumes/Data"));
    assert_eq!(m_data.fs_type, "apfs");

    let m_space =
        find_mount_for_path(Path::new("/Volumes/Grok Bot Installer/app"), &mounts).unwrap();
    assert_eq!(
        m_space.mount_point,
        Path::new("/Volumes/Grok Bot Installer")
    );
    assert_eq!(m_space.fs_type, "apfs");
    assert!(m_space.read_only);

    let m_root = find_mount_for_path(Path::new("/Users/alex/repo"), &mounts).unwrap();
    assert_eq!(m_root.mount_point, Path::new("/"));
    assert!(m_root.read_only);
}

#[test]
fn same_device_fallback_to_root_with_mismatched_dev_is_fail_closed() {
    // `/Volumes/Data2` is NOT in mount output, so longest path prefix falls back to `/` (apfs).
    // Because dev("/Volumes/Data2/repo") != dev("/"), is_apfs_mount_with_dev MUST return false.
    let only_root_and_data = parse_macos_mount_output(
        "/dev/disk3s8s1 on / (apfs, sealed, local, read-only)\n\
         /dev/disk3s7 on /Volumes/Data (apfs, local, journaled)\n",
    );
    let mount_devs = |mp: &Path| -> Option<u64> {
        if mp == Path::new("/") {
            Some(100)
        } else if mp == Path::new("/Volumes/Data") {
            Some(200)
        } else {
            None
        }
    };

    // Unlisted volume (dev 300) falls back to `/` (dev 100) -> MUST be false.
    assert!(!is_apfs_mount_with_dev(
        Path::new("/Volumes/Data2/repo"),
        300,
        &only_root_and_data,
        &mount_devs,
    ));

    // Real match on `/Volumes/Data` (dev 200 == 200, apfs) -> true.
    assert!(is_apfs_mount_with_dev(
        Path::new("/Volumes/Data/repo"),
        200,
        &only_root_and_data,
        &mount_devs,
    ));

    // Firmlink under `/` sharing dev 100 == dev("/") -> true.
    assert!(is_apfs_mount_with_dev(
        Path::new("/Users/alex/repo"),
        100,
        &only_root_and_data,
        &mount_devs,
    ));
}

#[test]
fn same_device_nonexistent_dst_uses_nearest_existing_ancestor_and_errors_on_enotdir() {
    let tmp = tempfile::tempdir().unwrap();
    let canon = fs::canonicalize(tmp.path()).unwrap();
    let nested_missing = tmp.path().join("does/not/exist/yet");

    let resolved = nearest_existing_canonical(&nested_missing).unwrap();
    assert_eq!(resolved, canon);

    // ENOTDIR (child of a regular file) must NOT silently climb to parent; must return Err.
    let reg_file = tmp.path().join("regular.txt");
    fs::write(&reg_file, "hi").unwrap();
    let through_file = reg_file.join("child");
    assert!(
        nearest_existing_canonical(&through_file).is_err(),
        "nearest_existing_canonical must fail closed on non-NotFound errors (ENOTDIR)"
    );

    let src = tmp.path().join("repo/Library");
    fs::create_dir_all(&src).unwrap();
    assert!(same_cow_device(&src, &nested_missing).unwrap());
}

#[test]
fn seed_nested_mount_in_src_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("node_modules");
    let nested = src.join("pkg/mounted_sub");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("file.js"), "x").unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    // Same dev everywhere -> Ok.
    check_no_nested_mount_with(&src, None, deadline, &|_| Ok(77)).unwrap();

    // Subdirectory on a different dev -> Err("nested_mount").
    let err = check_no_nested_mount_with(&src, None, deadline, &|p| {
        if p == nested { Ok(88) } else { Ok(77) }
    })
    .unwrap_err();
    assert!(err.starts_with("nested_mount"), "unexpected err: {err}");

    // Finding #9g: depth > MAX_WALK_DEPTH (128) fails closed with `nested_mount_max_depth`.
    let mut deep = tmp.path().join("deep");
    for i in 0..131 {
        deep.push(format!("d{i}"));
    }
    fs::create_dir_all(&deep).unwrap();
    let deep_err =
        check_no_nested_mount_with(&tmp.path().join("deep"), None, deadline, &|_| Ok(77))
            .unwrap_err();
    assert_eq!(deep_err, "nested_mount_max_depth");
}
