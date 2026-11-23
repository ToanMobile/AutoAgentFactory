#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]
//! What `aaf board`, `aaf accounts` and `aaf run` leave in the data dir is
//! readable by its owner only (the real binary, a data dir in a temp dir).

#[path = "factory_support.rs"]
mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use support::Fixture;

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// Every entry below `dir` that group or others can reach.
fn open_to_others(dir: &Path, found: &mut Vec<String>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if mode(&path) & 0o077 != 0 {
            found.push(format!("{} {:o}", path.display(), mode(&path)));
        }
        if path.is_dir() {
            open_to_others(&path, found);
        }
    }
}

#[test]
fn board_creates_an_owner_only_data_dir_and_database() {
    let fx = Fixture::new();

    fx.add("first task");

    assert_eq!(mode(&fx.data), 0o700);
    assert_eq!(mode(&fx.data.join("aaf.db")), 0o600);
}

#[test]
fn accounts_and_run_leave_nothing_others_can_read() {
    let fx = Fixture::new();

    let listed = fx.aaf(&["accounts", "list"]);
    assert_eq!(listed.code, 0, "{}", listed.stderr);
    assert_eq!(mode(&fx.data), 0o700);

    // Nothing to do, but the run lock and the store are opened. Provide a
    // stub `claude` binary on PATH so this test does not depend on a real
    // Claude CLI installation on CI runners.
    let bin = fx.root().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let stub = bin.join("claude");
    fs::write(&stub, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let ran = fx.aaf_in(&fx.repo, &["run", "--unsandboxed"], &[("PATH", &path)]);
    assert_eq!(ran.code, 0, "{}", ran.stderr);

    let mut found = Vec::new();
    open_to_others(&fx.data, &mut found);
    assert!(found.is_empty(), "{}", found.join("\n"));
    assert!(fx.data.join("locks").is_dir());
}
