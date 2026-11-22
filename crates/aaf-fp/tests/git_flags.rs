#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Flags every `aaf_fp::git` call carries.

use std::process::Command;

#[test]
fn fp_git_never_starts_auto_gc() {
    let dir = tempfile::tempdir().unwrap();
    let init = Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["init", "-q"])
        .env_remove("GIT_DIR")
        .output()
        .unwrap();
    assert!(init.status.success());
    // `git config --get` sees the `-c` values of its own invocation.
    let seen = aaf_fp::git(
        dir.path(),
        &["config", "--get", "gc.auto"],
        &[],
        aaf_fp::GIT_TIMEOUT,
    );
    assert_eq!(seen.ok().as_deref(), Some("0"));
    // The flag must not swallow the caller's own `-c` options.
    let name = aaf_fp::git(
        dir.path(),
        &["-c", "user.name=probe", "config", "--get", "user.name"],
        &[],
        aaf_fp::GIT_TIMEOUT,
    )
    .unwrap();
    assert_eq!(name, "probe");
}
