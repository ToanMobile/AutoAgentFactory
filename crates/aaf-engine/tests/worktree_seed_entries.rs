#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Strict entry-list cloning, existing-dst safety, entry validation, batching (300 args &
//! 64 KiB byte cap), batch-2 partial 0555 cleanup, and single-call strict dir clone.

use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::time::Duration;

use aaf_engine::git::hydration::stage::{cow_clone_dir_strict_with, cow_clone_entries_strict_with};

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
fn seed_strict_clone_existing_dst_skips_without_deleting_it() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("a.txt"), "new").unwrap();

    // 1. cow_clone_dir_strict_with checks dst before probe -> dst_already_exists on BOTH OSes.
    let dst1 = tmp.path().join("existing_dir");
    fs::create_dir_all(&dst1).unwrap();
    fs::write(dst1.join("sentinel.txt"), "keep").unwrap();
    let err1 = cow_clone_dir_strict_with(
        &src,
        &dst1,
        Duration::from_secs(10),
        Ok(true),
        "/bin/cp",
        None,
    )
    .unwrap_err();
    assert!(!err1.created);
    assert_eq!(err1.reason, "dst_already_exists");
    assert_eq!(
        fs::read_to_string(dst1.join("sentinel.txt")).unwrap(),
        "keep"
    );

    // 2. cow_clone_entries_strict_with creates dst before probe -> dst_already_exists on BOTH OSes.
    let dst2 = tmp.path().join("existing_entries_dir");
    fs::create_dir_all(&dst2).unwrap();
    fs::write(dst2.join("sentinel.txt"), "keep2").unwrap();
    let err2 = cow_clone_entries_strict_with(
        &src,
        &[OsString::from("a.txt")],
        &dst2,
        Duration::from_secs(10),
        Ok(true),
        "/bin/cp",
        None,
    )
    .unwrap_err();
    assert!(!err2.created);
    assert_eq!(err2.reason, "dst_already_exists");
    assert_eq!(
        fs::read_to_string(dst2.join("sentinel.txt")).unwrap(),
        "keep2"
    );

    // 3. Invalid entry names rejected before creating dst.
    for bad in ["../escape", "a/b", ".", "/abs", ""] {
        let dst_bad = tmp.path().join("should_not_exist");
        let err = cow_clone_entries_strict_with(
            &src,
            &[OsString::from(bad)],
            &dst_bad,
            Duration::from_secs(10),
            Ok(true),
            "/bin/cp",
            None,
        )
        .unwrap_err();
        assert!(!err.created);
        assert!(
            err.reason.starts_with("invalid_entry_name"),
            "{bad:?}: {}",
            err.reason
        );
        assert!(!dst_bad.exists());
    }
}

#[test]
fn seed_entries_strict_batching_300_64kib_cleanup_on_batch2_and_single_call_fail() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src_many");
    fs::create_dir_all(&src).unwrap();
    let mut entries = Vec::new();
    for i in 0..300 {
        let name = format!("item_{i:03}.txt");
        fs::write(src.join(&name), format!("v{i}")).unwrap();
        entries.push(OsString::from(name));
    }

    // 1a. 300 entries (> MAX_BATCH_ARGS 256) splits into >= 2 batches on BOTH macOS and Linux ext4.
    let log_ok = tmp.path().join("batch_ok.log");
    let cp_ok = tmp.path().join("cp_ok.sh");
    // Preserve spaces in paths; on Linux replace `--reflink=always` with `--reflink=never`
    // so ext4 CI runners exercise real 300-entry and 64 KiB batching end-to-end.
    make_executable(
        &cp_ok,
        &format!(
            "#!/bin/sh\n\
             echo call >> \"{}\"\n\
             if [ \"$(uname -s)\" = \"Darwin\" ]; then\n\
               exec /bin/cp \"$@\"\n\
             fi\n\
             n=$#\n\
             i=0\n\
             while [ \"$i\" -lt \"$n\" ]; do\n\
               arg=$1; shift\n\
               if [ \"$arg\" = \"--reflink=always\" ]; then arg=\"--reflink=never\"; fi\n\
               set -- \"$@\" \"$arg\"\n\
               i=$((i + 1))\n\
             done\n\
             exec /bin/cp \"$@\"\n",
            log_ok.display()
        ),
    );
    let dst_ok = tmp.path().join("dst_300");
    cow_clone_entries_strict_with(
        &src,
        &entries,
        &dst_ok,
        Duration::from_secs(15),
        Ok(true),
        cp_ok.to_str().unwrap(),
        Some(true),
    )
    .unwrap();
    let calls = fs::read_to_string(&log_ok).unwrap().lines().count();
    assert!(
        calls >= 2,
        "expected >= 2 batches for 300 entries, got {calls}"
    );
    assert_eq!(
        fs::read_to_string(dst_ok.join("item_000.txt")).unwrap(),
        "v0"
    );
    assert_eq!(
        fs::read_to_string(dst_ok.join("item_299.txt")).unwrap(),
        "v299"
    );

    // 1b. Finding #9g: 64 KiB byte limit splits into >= 2 batches even with < 256 entries.
    // 250 entries × (~230-byte filename + ~50-byte dir path) ≈ 70 KiB > 65,536 bytes.
    let src_long = tmp.path().join("src_long_names_padding_dir_for_64kib");
    fs::create_dir_all(&src_long).unwrap();
    let long_pad = "x".repeat(220);
    let mut long_entries = Vec::new();
    for i in 0..250 {
        let name = format!("f_{i:03}_{long_pad}.txt");
        fs::write(src_long.join(&name), "b").unwrap();
        long_entries.push(OsString::from(name));
    }
    let _ = fs::remove_file(&log_ok);
    let dst_long = tmp.path().join("dst_64kib");
    cow_clone_entries_strict_with(
        &src_long,
        &long_entries,
        &dst_long,
        Duration::from_secs(15),
        Ok(true),
        cp_ok.to_str().unwrap(),
        Some(true),
    )
    .unwrap();
    let long_calls = fs::read_to_string(&log_ok).unwrap().lines().count();
    assert!(
        long_calls >= 2,
        "expected >= 2 batches due to 64 KiB limit (< 256 entries), got {long_calls}"
    );
    assert!(dst_long.join(&long_entries[0]).is_file());
    assert!(dst_long.join(&long_entries[249]).is_file());

    // 2. Failure on batch 2 deletes dst including 0555 output from batch 1 (reflink_ok=Some(true) skips probe).
    let count_file = tmp.path().join("batch_count");
    let cp_fail2 = tmp.path().join("cp_fail2.sh");
    make_executable(
        &cp_fail2,
        &format!(
            "#!/bin/sh\n\
             eval \"dst=\\$$#\"\n\
             n=0\n\
             if [ -f \"{cf}\" ]; then n=$(cat \"{cf}\"); fi\n\
             n=$((n + 1))\n\
             echo $n > \"{cf}\"\n\
             if [ \"$n\" -eq 1 ]; then\n\
               mkdir -p \"$dst/b1_ro\"\n\
               printf 'b1' > \"$dst/b1_ro/f\"\n\
               chmod 555 \"$dst/b1_ro\"\n\
               exit 0\n\
             fi\n\
             echo 'batch 2 failed' >&2\n\
             exit 1\n",
            cf = count_file.display()
        ),
    );
    let dst_fail2 = tmp.path().join("dst_fail2");
    let err = cow_clone_entries_strict_with(
        &src,
        &entries,
        &dst_fail2,
        Duration::from_secs(15),
        Ok(true),
        cp_fail2.to_str().unwrap(),
        Some(true),
    )
    .unwrap_err();
    assert!(err.created);
    assert!(err.reason.starts_with("cp_failed"), "{}", err.reason);
    assert!(fs::symlink_metadata(&dst_fail2).is_err());

    // 3. Strict dir clone calls cp only once and never falls back to deep copy.
    let log_strict = tmp.path().join("strict_calls.log");
    let cp_reject_cow = tmp.path().join("cp_reject_cow.sh");
    make_executable(
        &cp_reject_cow,
        &format!(
            "#!/bin/sh\n\
             echo \"$*\" >> \"{}\"\n\
             for a in \"$@\"; do\n\
               case \"$a\" in -cRP|--reflink=always) echo 'no cow' >&2; exit 1 ;; esac\n\
             done\n\
             exec /bin/cp \"$@\"\n",
            log_strict.display()
        ),
    );
    let dst_strict = tmp.path().join("dst_strict_once");
    let err_s = cow_clone_dir_strict_with(
        &src,
        &dst_strict,
        Duration::from_secs(10),
        Ok(true),
        cp_reject_cow.to_str().unwrap(),
        Some(true),
    )
    .unwrap_err();
    assert!(!dst_strict.exists());
    assert!(err_s.reason.starts_with("cp_failed"), "{}", err_s.reason);
    assert_eq!(fs::read_to_string(&log_strict).unwrap().lines().count(), 1);
}
