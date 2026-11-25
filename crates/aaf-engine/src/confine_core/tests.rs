#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use super::path;
use super::shell::{
    ConfineMode, REASON_NO_DATA_DIR, bare_sh, build_bwrap_argv, build_seatbelt_profile,
    linux_shell_argv_with_probe, macos_shell_argv, shell_argv,
};

fn canon_tempdir() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    (tmp, root)
}

#[test]
fn seatbelt_profile_rules_and_ancestors_for_normal_path() {
    let (_tmp, root) = canon_tempdir();
    let data = root.join("p/data");
    let repo = root.join("repo");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&repo).unwrap();

    let profile = build_seatbelt_profile(&data, &repo).unwrap();
    let d = data.to_str().unwrap();
    assert!(profile.starts_with("(version 1)\n(allow default)\n"));
    assert!(profile.contains(&format!(
        "(deny file-write* (literal \"{}\"))\n",
        root.join("p").display()
    )));
    assert!(profile.contains(&format!("(deny file-write* (literal \"{d}\"))\n")));
    assert!(profile.contains(&format!(
        "(deny file-read* file-write* (subpath \"{d}\"))\n"
    )));
    assert!(profile.contains(&format!("(allow file-read* (subpath \"{d}/throttle\"))\n")));
    assert!(profile.contains(&format!(
        "(allow file-read* file-write* (subpath \"{d}/throttle/guest\"))\n"
    )));
    assert!(profile.contains(&format!(
        "(deny file-write* (literal \"{d}/throttle/guest\"))\n"
    )));
    let escaped = path::regex_escape(d);
    assert!(profile.contains(&format!(
        "(allow file-read* (regex #\"^{escaped}/locks/heavy\\.[0-9]+\\.token$\"))\n"
    )));
    assert!(!profile.contains(&format!("(subpath \"{}\")", repo.display())));
}

#[test]
fn seatbelt_profile_guards_leaf_intermediate_and_two_hop_symlinks() {
    let (_tmp, root) = canon_tempdir();
    let repo = root.join("repo");
    let real = root.join("dir3/real");
    let dir1 = root.join("dir1");
    let dir2 = root.join("dir2");
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir_all(&real).unwrap();
    fs::create_dir_all(&dir1).unwrap();
    fs::create_dir_all(&dir2).unwrap();

    // 1-hop leaf symlink
    let leaf_link = dir1.join("leaf");
    symlink(&real, &leaf_link).unwrap();
    let p_leaf = build_seatbelt_profile(&leaf_link, &repo).unwrap();
    assert!(p_leaf.contains(&format!(
        "(deny file-write* (literal \"{}\"))\n",
        leaf_link.display()
    )));
    assert!(p_leaf.contains(&format!(
        "(deny file-read* file-write* (subpath \"{}\"))\n",
        real.display()
    )));

    // Intermediate symlink
    let mid_real = root.join("mid_real");
    fs::create_dir_all(mid_real.join("data")).unwrap();
    let mid_link = root.join("mid_link");
    symlink(&mid_real, &mid_link).unwrap();
    let through_mid = mid_link.join("data");
    let p_mid = build_seatbelt_profile(&through_mid, &repo).unwrap();
    assert!(p_mid.contains(&format!(
        "(deny file-write* (literal \"{}\"))\n",
        mid_link.display()
    )));
    assert!(p_mid.contains(&format!(
        "(deny file-write* (literal \"{}\"))\n",
        mid_real.join("data").display()
    )));

    // 2-hop symlink chain: dir1/link1 -> dir2/link2 -> dir3/real
    let link2 = dir2.join("link2");
    let link1 = dir1.join("link1");
    symlink(&real, &link2).unwrap();
    symlink(&link2, &link1).unwrap();
    let p_chain = build_seatbelt_profile(&link1, &repo).unwrap();
    for expected in [&link1, &dir1, &link2, &dir2, &real, &root.join("dir3")] {
        assert!(
            p_chain.contains(&format!(
                "(deny file-write* (literal \"{}\"))\n",
                expected.display()
            )),
            "missing literal guard for {} in:\n{p_chain}",
            expected.display()
        );
    }
}

#[test]
fn seatbelt_profile_quotes_spaces_and_regex_special_chars() {
    let (_tmp, root) = canon_tempdir();
    let data = root.join("my data.v1+test");
    let repo = root.join("repo");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&repo).unwrap();

    let profile = build_seatbelt_profile(&data, &repo).unwrap();
    let d = data.to_str().unwrap();
    assert!(profile.contains(&format!(
        "(deny file-read* file-write* (subpath \"{d}\"))\n"
    )));
    assert!(profile.contains("my data\\.v1\\+test/locks/heavy\\.[0-9]+\\.token$"));
}

#[test]
fn rejects_forbidden_characters_in_paths() {
    let (_tmp, root) = canon_tempdir();
    let data = root.join("data");
    let repo = root.join("repo");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&repo).unwrap();

    for bad in ["bad\"quote", "bad\\slash", "bad\nline", "bad\x07ctrl"] {
        let bad_path = root.join(bad);
        assert!(build_seatbelt_profile(&bad_path, &repo).is_err(), "{bad:?}");
        assert!(build_seatbelt_profile(&data, &bad_path).is_err(), "{bad:?}");
        assert!(
            build_bwrap_argv(Path::new("/usr/bin/bwrap"), "true", &repo, &bad_path).is_err(),
            "{bad:?}"
        );
    }
}

#[test]
fn cwd_rules_reject_data_dir_and_locks_but_reopen_warm_slots() {
    let (_tmp, root) = canon_tempdir();
    let data = root.join("data");
    let locks = data.join("locks");
    let slot = data.join("warm-slots/oracle/slot-0");
    fs::create_dir_all(&locks).unwrap();
    fs::create_dir_all(&slot).unwrap();

    assert!(build_seatbelt_profile(&data, &data).is_err());
    assert!(build_seatbelt_profile(&data, &locks).is_err());
    assert!(build_bwrap_argv(Path::new("/usr/bin/bwrap"), "true", &data, &data).is_err());
    assert!(build_bwrap_argv(Path::new("/usr/bin/bwrap"), "true", &locks, &data).is_err());

    let profile = build_seatbelt_profile(&data, &slot).unwrap();
    assert!(profile.contains(&format!(
        "(allow file-read* file-write* (subpath \"{}\"))\n",
        slot.display()
    )));
}

#[test]
fn none_or_missing_data_dir_returns_bare_sh() {
    let (_tmp, root) = canon_tempdir();
    let repo = root.join("repo");
    fs::create_dir_all(&repo).unwrap();

    let (argv, mode) = shell_argv("echo hi", &repo, None).unwrap();
    assert_eq!(
        argv,
        vec![OsString::from("/bin/sh"), "-c".into(), "echo hi".into()]
    );
    assert_eq!(
        mode,
        ConfineMode::Unconfined {
            reason: REASON_NO_DATA_DIR.to_string()
        }
    );

    let missing = root.join("does-not-exist");
    let (argv2, mode2) = shell_argv("echo hi", &repo, Some(&missing)).unwrap();
    assert_eq!(argv2, argv);
    assert_eq!(mode2, mode);
}

#[test]
fn macos_missing_sandbox_exec_fails_closed() {
    let (_tmp, root) = canon_tempdir();
    let data = root.join("data");
    let repo = root.join("repo");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&repo).unwrap();

    let err =
        macos_shell_argv("true", &repo, &data, Path::new("/nonexistent/sandbox-exec")).unwrap_err();
    assert!(
        err.to_string()
            .starts_with("gate commands cannot be confined:")
    );
}

#[test]
fn bwrap_argv_pins_ancestors_shallow_to_deep_before_tmpfs_and_rejects_symlinks() {
    let (_tmp, root) = canon_tempdir();
    let data = root.join("a/b/data");
    let repo = root.join("repo");
    fs::create_dir_all(data.join("throttle/guest")).unwrap();
    fs::create_dir_all(data.join("locks")).unwrap();
    fs::write(data.join("locks/heavy.0.token"), "token=abc\n").unwrap();
    fs::write(data.join("locks/repo.run.lock"), "").unwrap();
    fs::create_dir_all(&repo).unwrap();

    let argv = build_bwrap_argv(Path::new("/usr/bin/bwrap"), "echo ok", &repo, &data).unwrap();
    let strs: Vec<&str> = argv.iter().map(|s| s.to_str().unwrap()).collect();

    assert_eq!(strs[0], "/usr/bin/bwrap");
    assert!(strs.contains(&"--die-with-parent"));
    assert!(!strs.contains(&"--new-session"));

    let tmpfs_pos = strs.iter().position(|&s| s == "--tmpfs").unwrap();
    assert_eq!(strs[tmpfs_pos + 1], data.to_str().unwrap());

    let mut expected_ancestors: Vec<&Path> = data
        .ancestors()
        .skip(1)
        .filter(|p| p.parent().is_some())
        .collect();
    expected_ancestors.reverse();
    assert!(expected_ancestors.len() >= 2);
    let mut prev_pos = 0usize;
    for anc in &expected_ancestors {
        let anc_s = anc.to_str().unwrap();
        let pos = strs
            .windows(3)
            .position(|w| w == ["--bind", anc_s, anc_s])
            .unwrap_or_else(|| panic!("missing --bind for {anc_s}: {strs:?}"));
        assert!(pos < tmpfs_pos, "--bind {anc_s} must precede --tmpfs");
        assert!(pos > prev_pos, "ancestors must be ordered shallow->deep");
        prev_pos = pos;
    }
    assert!(strs.windows(3).any(|w| w
        == [
            "--ro-bind",
            data.join("locks/heavy.0.token").to_str().unwrap(),
            data.join("locks/heavy.0.token").to_str().unwrap(),
        ]));
    assert!(!strs.iter().any(|s| s.contains("repo.run.lock")));
    assert!(
        strs.windows(2)
            .any(|w| w == ["--chdir", repo.to_str().unwrap()])
    );

    // Symlinked data dir -> ConfineError when bwrap probe succeeded.
    let link = root.join("data_link");
    symlink(&data, &link).unwrap();
    let err = build_bwrap_argv(Path::new("/usr/bin/bwrap"), "true", &repo, &link).unwrap_err();
    assert_eq!(
        err.0,
        "data dir path goes through a symlink; set AAF_DATA_DIR to its resolved path"
    );

    // When bwrap probe fails, symlink-prefix check is skipped and mode is Unconfined.
    let (unconf_argv, mode) =
        linux_shell_argv_with_probe("true", &repo, &link, Err("bwrap missing")).unwrap();
    assert_eq!(unconf_argv, bare_sh("true"));
    assert_eq!(
        mode,
        ConfineMode::Unconfined {
            reason: "bwrap missing".to_string()
        }
    );
}
