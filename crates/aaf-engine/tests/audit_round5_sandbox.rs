#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for Round 5A `linux_sandbox` hardening.

use std::fs;
use std::path::{Path, PathBuf};

use aaf_engine::executors::linux_sandbox::build_bwrap_args;
use aaf_engine::executors::sandbox::SandboxPaths;
use tempfile::TempDir;

fn pos3(argv: &[String], a: &str, b: &str, c: &str) -> Option<usize> {
    argv.windows(3)
        .position(|w| w[0] == a && w[1] == b && w[2] == c)
}

fn pos2(argv: &[String], a: &str, b: &str) -> Option<usize> {
    argv.windows(2).position(|w| w[0] == a && w[1] == b)
}

#[test]
fn bwrap_skips_normalized_tmp_variants_while_binding_tmp_subdirs() {
    let tmp_sub = tempfile::tempdir_in("/tmp").unwrap();
    let sub_str = tmp_sub.path().to_string_lossy().into_owned();
    let tmp_canon = fs::canonicalize("/tmp")
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
        .to_string_lossy()
        .into_owned();
    let paths = SandboxPaths {
        writable: vec![
            "/tmp".into(),
            "/tmp/".into(),
            "/tmp/.".into(),
            tmp_canon.clone(),
            sub_str.clone(),
        ],
        worktree: Some(sub_str.clone()),
        ..SandboxPaths::default()
    };
    let argv = build_bwrap_args(&paths, "/bin/true", &[]).unwrap();
    assert!(pos2(&argv, "--tmpfs", "/tmp").is_some(), "{argv:?}");
    for bad in ["/tmp", "/tmp/", "/tmp/.", tmp_canon.as_str()] {
        assert!(pos3(&argv, "--bind", bad, bad).is_none(), "{bad}: {argv:?}");
    }
    assert!(pos3(&argv, "--bind", &sub_str, &sub_str).is_some());
}

#[test]
fn bwrap_pins_worktrees_root_and_read_denied_ancestors_without_nested_tmpfs_hole() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let (wt_a, wt_b) = (root.join("nested_a"), root.join("nested_a/nested_b"));
    let (wt_root, wt) = (wt_b.join("worktrees"), wt_b.join("worktrees/repo/t1"));
    let (data_parent, data_dir) = (root.join("nested_c"), root.join("nested_c/aaf-data"));
    let nested_sub = data_dir.join("sub");
    let nested_secret = nested_sub.join("receipt.key");
    let (file_parent, secret_file) = (root.join("nested_d"), root.join("nested_d/secret.key"));

    for d in [&wt, &nested_sub, &file_parent] {
        fs::create_dir_all(d).unwrap();
    }
    fs::write(&nested_secret, "key").unwrap();
    fs::write(&secret_file, "secret").unwrap();

    let s = |p: &Path| p.to_string_lossy().into_owned();
    let paths = SandboxPaths {
        writable: vec![s(&root)],
        worktree: Some(s(&wt)),
        worktrees_root: Some(s(&wt_root)),
        read_denied: vec![s(&data_dir), s(&nested_secret), s(&secret_file)],
        ..SandboxPaths::default()
    };
    let argv = build_bwrap_args(&paths, "/bin/true", &[]).unwrap();
    let bind_pos = |p: &Path| pos3(&argv, "--bind", p.to_str().unwrap(), p.to_str().unwrap());
    let ro_pos = |p: &Path| pos3(&argv, "--ro-bind", p.to_str().unwrap(), p.to_str().unwrap());
    assert!(bind_pos(&wt_a).unwrap() < bind_pos(&wt_b).unwrap());
    assert!(bind_pos(&wt_b).unwrap() < ro_pos(&wt_root).unwrap());
    let tmpfs_pos = pos2(&argv, "--tmpfs", data_dir.to_str().unwrap()).unwrap();
    assert!(bind_pos(&data_parent).unwrap() < tmpfs_pos);
    let null_pos = pos3(
        &argv,
        "--ro-bind",
        "/dev/null",
        secret_file.to_str().unwrap(),
    )
    .unwrap();
    assert!(bind_pos(&file_parent).unwrap() < null_pos);
    assert!(bind_pos(&nested_sub).is_none(), "{argv:?}");
}

#[cfg(unix)]
#[test]
fn bwrap_handles_trailing_slash_and_dot_on_leaf_symlinks() {
    use std::os::unix::fs::symlink;

    let dir = TempDir::new().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let real_wt_root = root.join("real/worktrees");
    let wt = real_wt_root.join("repo/t1");
    let real_data = root.join("real/aaf-data");
    let throttle = real_data.join("throttle");
    let guest = throttle.join("guest");
    for d in [&wt, &guest] {
        fs::create_dir_all(d).unwrap();
    }
    let links = root.join("links");
    fs::create_dir_all(&links).unwrap();
    let wt_link = links.join("worktrees");
    let data_link = links.join("aaf-data");
    symlink(&real_wt_root, &wt_link).unwrap();
    symlink(&real_data, &data_link).unwrap();

    let s = |p: &Path| p.to_string_lossy().into_owned();
    let wt_with_slash = format!("{}/", s(&wt_link));
    let data_with_dot = format!("{}/.", s(&data_link));
    let paths = SandboxPaths {
        writable: vec![s(&root)],
        worktree: Some(s(&wt)),
        worktrees_root: Some(wt_with_slash.clone()),
        read_denied: vec![data_with_dot.clone()],
        ..SandboxPaths::default()
    };
    let argv = build_bwrap_args(&paths, "/bin/true", &[]).unwrap();
    assert!(pos3(&argv, "--ro-bind", &wt_with_slash, &wt_with_slash).is_none());
    assert!(pos3(&argv, "--ro-bind", &s(&real_wt_root), &s(&real_wt_root)).is_some());
    assert!(pos2(&argv, "--tmpfs", &data_with_dot).is_none());
    assert!(pos2(&argv, "--tmpfs", &s(&real_data)).is_some());
    assert!(pos3(&argv, "--bind", &s(&guest), &s(&guest)).is_some());
}
