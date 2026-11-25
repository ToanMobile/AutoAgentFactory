#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Step 2 & Round-2 (Findings #1-#4, #8, #9g, #11) tests: strict vs fallback CoW clone,
//! cross-device behavior, Linux strict probe-in-dst & clone_dirs fallback without probe,
//! batching (arg count & 64 KiB), timeout/error cleanup on every branch, and read-only (`0555`) recovery.

use std::fs;
use std::path::Path;
use std::time::Duration;

use aaf_engine::git::hydration::device::{device_id, same_cow_device};
use aaf_engine::git::hydration::stage::{
    cow_clone_dir_strict_with, cow_clone_dir_with, remove_partial_dst,
};

fn make_executable(path: &Path, body: &str) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let staged = path.with_extension("staged");
        fs::write(&staged, body).unwrap();
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755)).unwrap();
        fs::rename(&staged, path).unwrap();
    }
}

#[test]
fn seed_strict_clone_cross_device_returns_skip_without_creating_dst() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("a.txt"), "hello").unwrap();
    let dst = tmp.path().join("dst");

    let err = cow_clone_dir_strict_with(
        &src,
        &dst,
        Duration::from_secs(10),
        Ok(false),
        "/bin/cp",
        None,
    )
    .unwrap_err();
    assert!(!err.created, "cross-device skip must have created=false");
    assert!(
        err.reason.starts_with("cross_device"),
        "unexpected skip reason: {}",
        err.reason
    );
    assert!(!dst.exists(), "strict clone must not create dst");

    let manifest_src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    if let (Ok(d1), Ok(d2)) = (device_id(&manifest_src), device_id(tmp.path()))
        && d1 != d2
    {
        let real_dst = tmp.path().join("real-cross-dst");
        let same_dev = same_cow_device(&manifest_src, tmp.path()).map_err(|e| e.to_string());
        let real_err = cow_clone_dir_strict_with(
            &manifest_src,
            &real_dst,
            Duration::from_secs(10),
            same_dev,
            "/bin/cp",
            None,
        )
        .unwrap_err();
        assert!(!real_err.created);
        assert!(
            !real_dst.exists(),
            "real cross-device strict clone must not copy: {real_err}"
        );
    }
}

#[test]
fn seed_clone_dirs_cross_device_copies_with_warning() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir_all(src.join("sub")).unwrap();
    fs::write(src.join("sub/file.txt"), "data").unwrap();
    let dst = tmp.path().join("dst");

    let cp_bin: String = if cfg!(target_os = "macos") {
        "/bin/cp".to_string()
    } else {
        let fake_cp = tmp.path().join("fake_cp_no_reflink.sh");
        make_executable(
            &fake_cp,
            "#!/bin/sh\n\
             for a in \"$@\"; do\n\
               if [ \"$a\" = \"--reflink=always\" ]; then\n\
                 echo 'reflink unsupported' >&2\n\
                 exit 1\n\
               fi\n\
             done\n\
             exec /bin/cp \"$@\"\n",
        );
        fake_cp.to_string_lossy().into_owned()
    };

    let mut warnings = Vec::new();
    cow_clone_dir_with(
        &src,
        &dst,
        Duration::from_secs(10),
        Ok(false),
        &cp_bin,
        &mut |msg| warnings.push(msg.to_string()),
    )
    .unwrap();

    assert_eq!(
        fs::read_to_string(dst.join("sub/file.txt")).unwrap(),
        "data"
    );
    assert!(!warnings.is_empty(), "expected fallback warning");
}

#[cfg(target_os = "macos")]
#[test]
fn seed_clone_dirs_macos_device_check_err_and_fallback_rp_timeout() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("a.txt"), "cow").unwrap();
    let dst = tmp.path().join("dst");
    let log = tmp.path().join("cp.log");

    let fake_cp = tmp.path().join("log_cp.sh");
    make_executable(
        &fake_cp,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"{}\"\nexec /bin/cp \"$@\"\n",
            log.display()
        ),
    );

    let mut warnings = Vec::new();
    cow_clone_dir_with(
        &src,
        &dst,
        Duration::from_secs(10),
        Err("mount timed out".to_string()),
        fake_cp.to_str().unwrap(),
        &mut |w| warnings.push(w.to_string()),
    )
    .unwrap();
    assert_eq!(fs::read_to_string(dst.join("a.txt")).unwrap(), "cow");
    let logged = fs::read_to_string(&log).unwrap();
    assert!(
        logged.lines().next().unwrap_or("").contains("-cRP"),
        "{logged}"
    );
    assert!(!warnings.is_empty());

    // Finding #9g: primary `cp -cRP` fails immediately, then fallback `cp -RP` times out
    // with a partial 0555 dir -> must return Timeout and delete partial dst.
    let fb_slow = tmp.path().join("fb_slow_cp.sh");
    make_executable(
        &fb_slow,
        "#!/bin/sh\n\
         eval \"dst=\\$$#\"\n\
         for a in \"$@\"; do\n\
           if [ \"$a\" = \"-cRP\" ]; then echo 'cRP fail' >&2; exit 1; fi\n\
         done\n\
         mkdir -p \"$dst/ro_fb\"\n\
         printf 'partial' > \"$dst/ro_fb/f\"\n\
         chmod 555 \"$dst/ro_fb\"\n\
         sleep 10\n",
    );
    let dst_fb = tmp.path().join("dst_fb_rp_timeout");
    let err = cow_clone_dir_with(
        &src,
        &dst_fb,
        Duration::from_millis(200),
        Ok(true),
        fb_slow.to_str().unwrap(),
        &mut |_| {},
    )
    .unwrap_err();
    assert!(
        matches!(err, aaf_engine::git::GitError::Timeout { .. }),
        "expected Timeout on fallback cp -RP, got {err:?}"
    );
    assert!(
        fs::symlink_metadata(&dst_fb).is_err(),
        "partial 0555 dst must be removed after fallback cp -RP timeout"
    );
}

#[test]
fn seed_clone_timeout_and_failure_cleanup_partial_tree_including_0555() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("a.txt"), "x").unwrap();

    let fail_cp = tmp.path().join("fail_cp.sh");
    make_executable(
        &fail_cp,
        "#!/bin/sh\n\
         eval \"dst=\\$$#\"\n\
         mkdir -p \"$dst/ro_sub\"\n\
         printf 'partial' > \"$dst/ro_sub/f.txt\"\n\
         chmod 444 \"$dst/ro_sub/f.txt\"\n\
         chmod 555 \"$dst/ro_sub\"\n\
         chmod 555 \"$dst\"\n\
         echo 'simulated cp failure' >&2\n\
         exit 1\n",
    );

    // 1. Strict clone failure removes 0555 partial tree (reflink_ok=Some(true) bypasses probe).
    let dst_strict_fail = tmp.path().join("dst_strict_fail");
    let err = cow_clone_dir_strict_with(
        &src,
        &dst_strict_fail,
        Duration::from_secs(5),
        Ok(true),
        fail_cp.to_str().unwrap(),
        Some(true),
    )
    .unwrap_err();
    assert!(!err.cleanup_failed, "{}", err.reason);
    assert!(fs::symlink_metadata(&dst_strict_fail).is_err());

    // 2. Fallback clone_dirs failure removes 0555 partial tree.
    let dst_fb_fail = tmp.path().join("dst_fb_fail");
    assert!(
        cow_clone_dir_with(
            &src,
            &dst_fb_fail,
            Duration::from_secs(5),
            Ok(false),
            fail_cp.to_str().unwrap(),
            &mut |_| {},
        )
        .is_err()
    );
    assert!(fs::symlink_metadata(&dst_fb_fail).is_err());

    // 3. Timeout removes 0555 partial tree on both strict and clone_dirs.
    let slow_cp = tmp.path().join("slow_cp.sh");
    make_executable(
        &slow_cp,
        "#!/bin/sh\n\
         eval \"dst=\\$$#\"\n\
         mkdir -p \"$dst/ro_sub\"\n\
         printf 'partial' > \"$dst/ro_sub/f.txt\"\n\
         chmod 555 \"$dst/ro_sub\"\n\
         sleep 10\n",
    );

    let dst_strict_to = tmp.path().join("dst_strict_timeout");
    let to_err = cow_clone_dir_strict_with(
        &src,
        &dst_strict_to,
        Duration::from_millis(200),
        Ok(true),
        slow_cp.to_str().unwrap(),
        Some(true),
    )
    .unwrap_err();
    assert!(to_err.reason.contains("timeout"), "{}", to_err.reason);
    assert!(fs::symlink_metadata(&dst_strict_to).is_err());

    let dst_fb_to = tmp.path().join("dst_fb_timeout");
    let fb_to_err = cow_clone_dir_with(
        &src,
        &dst_fb_to,
        Duration::from_millis(200),
        Ok(true),
        slow_cp.to_str().unwrap(),
        &mut |_| {},
    )
    .unwrap_err();
    assert!(matches!(
        fb_to_err,
        aaf_engine::git::GitError::Timeout { .. }
    ));
    assert!(fs::symlink_metadata(&dst_fb_to).is_err());
}

#[cfg(unix)]
#[test]
fn seed_remove_partial_dst_recovers_0555_without_following_symlinks() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let tmp = tempfile::tempdir().unwrap();
    let outside = tmp.path().join("outside_secret.txt");
    fs::write(&outside, "keep-mode").unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o400)).unwrap();

    let tree = tmp.path().join("partial");
    let nested = tree.join("ro_dir");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("inner.txt"), "bye").unwrap();
    symlink(&outside, nested.join("link_out")).unwrap();
    fs::set_permissions(&nested, fs::Permissions::from_mode(0o555)).unwrap();
    fs::set_permissions(&tree, fs::Permissions::from_mode(0o555)).unwrap();

    remove_partial_dst(&tree).unwrap();
    assert!(fs::symlink_metadata(&tree).is_err());
    let outside_mode = fs::metadata(&outside).unwrap().permissions().mode() & 0o777;
    assert_eq!(outside_mode, 0o400);
}

#[cfg(target_os = "linux")]
#[test]
fn seed_linux_clone_dirs_fallback_and_probe_stays_inside_dst() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("item.txt"), "linux-data").unwrap();
    let parent_dir = tmp.path().join("wt");
    fs::create_dir_all(&parent_dir).unwrap();
    let dst = parent_dir.join("dst");
    let log = tmp.path().join("cp_args.log");

    let fake_cp = tmp.path().join("fake_cp.sh");
    make_executable(
        &fake_cp,
        &format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> \"{}\"\n\
             for a in \"$@\"; do\n\
               if [ \"$a\" = \"--reflink=always\" ]; then\n\
                 echo 'reflink not supported' >&2\n\
                 exit 1\n\
               fi\n\
             done\n\
             exec /bin/cp \"$@\"\n",
            log.display()
        ),
    );

    let mut warnings = Vec::new();
    cow_clone_dir_with(
        &src,
        &dst,
        Duration::from_secs(10),
        Ok(true),
        fake_cp.to_str().unwrap(),
        &mut |w| warnings.push(w.to_string()),
    )
    .unwrap();

    assert_eq!(
        fs::read_to_string(dst.join("item.txt")).unwrap(),
        "linux-data"
    );
    assert!(!warnings.is_empty());
    let logged = fs::read_to_string(&log).unwrap();
    assert!(logged.contains("--reflink=always"), "{logged}");
    assert!(logged.contains("--reflink=never"), "{logged}");
    // Legacy clone_dirs must never create a reflink probe file or call cp with one.
    assert!(
        !logged.contains(".aaf-reflink-probe"),
        "clone_dirs must not run a reflink probe:\n{logged}"
    );
    let parent_entries: Vec<_> = fs::read_dir(&parent_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(parent_entries, vec![std::ffi::OsString::from("dst")]);

    // Strict clone with reflink_ok=None probes inside dst, skips reflink_unsupported, leaves wt clean.
    let dst_strict = parent_dir.join("dst_strict");
    let err = cow_clone_dir_strict_with(
        &src,
        &dst_strict,
        Duration::from_secs(10),
        Ok(true),
        fake_cp.to_str().unwrap(),
        None,
    )
    .unwrap_err();
    assert_eq!(err.reason, "reflink_unsupported");
    assert!(!dst_strict.exists());
    let logged_after_strict = fs::read_to_string(&log).unwrap();
    assert!(
        logged_after_strict.contains(&format!("{}/.aaf-reflink-probe", dst_strict.display())),
        "strict clone probe must run inside dst_strict:\n{logged_after_strict}"
    );
    let parent_entries_after: Vec<_> = fs::read_dir(&parent_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(parent_entries_after, vec![std::ffi::OsString::from("dst")]);
}

#[cfg(unix)]
#[test]
fn seed_clone_dirs_never_probes_and_never_creates_dst_before_first_cp() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("item.txt"), "payload").unwrap();
    let parent_dir = tmp.path().join("wt");
    fs::create_dir_all(&parent_dir).unwrap();
    let dst = parent_dir.join("dst");
    let log = tmp.path().join("cp_args.log");
    let first_marker = tmp.path().join("first_cp.marker");
    let dst_state = tmp.path().join("dst_state.log");

    let fake_cp = tmp.path().join("fake_cp_record.sh");
    make_executable(
        &fake_cp,
        &format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> \"{}\"\n\
             if [ ! -f \"{}\" ]; then\n\
               : > \"{}\"\n\
               if [ -e \"{}\" ] || [ -L \"{}\" ]; then\n\
                 printf 'dst_existed' > \"{}\"\n\
               else\n\
                 printf 'dst_absent' > \"{}\"\n\
               fi\n\
             fi\n\
             exec /bin/cp -RP -- \"{}\" \"{}\"\n",
            log.display(),
            first_marker.display(),
            first_marker.display(),
            dst.display(),
            dst.display(),
            dst_state.display(),
            dst_state.display(),
            src.display(),
            dst.display(),
        ),
    );

    let mut warnings = Vec::new();
    cow_clone_dir_with(
        &src,
        &dst,
        Duration::from_secs(10),
        Ok(true),
        fake_cp.to_str().unwrap(),
        &mut |w| warnings.push(w.to_string()),
    )
    .unwrap();

    assert_eq!(fs::read_to_string(dst.join("item.txt")).unwrap(), "payload");
    assert!(
        warnings.is_empty(),
        "primary fake cp succeeded; expected no fallback warnings: {warnings:?}"
    );
    assert_eq!(
        fs::read_to_string(&dst_state).unwrap(),
        "dst_absent",
        "clone_dirs must not create dst before the first cp invocation"
    );
    let logged = fs::read_to_string(&log).unwrap();
    assert!(
        !logged.contains(".aaf-reflink-probe"),
        "clone_dirs must never invoke cp with a .aaf-reflink-probe path:\n{logged}"
    );
    if cfg!(target_os = "macos") {
        assert!(logged.contains("-cRP"), "expected -cRP on macOS:\n{logged}");
    } else {
        assert!(
            logged.contains("--reflink=always"),
            "expected --reflink=always on Linux:\n{logged}"
        );
    }
}
