//! Writing a program a test then runs (included with `#[path]` by the
//! `aaf-fp` and `aaf-engine` tests; as a test target of its own it holds no
//! tests). Standard library only.
//!
//! Why not `fs::write` + `chmod`: the test harness runs tests on many
//! threads, and every `Command::spawn` forks. A fork that lands while
//! `fs::write` still holds its descriptor on the new file hands the child a
//! copy of that writable descriptor, which lives until the child's `exec`
//! closes it (close-on-exec). Executing the file in that window fails on
//! Linux with `ETXTBSY` ("Text file busy", os error 26). Here the only
//! writer of the program's inode is a `/bin/cp` process that has exited
//! before this returns, so no thread of the test process can leak one.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

/// Creates (or replaces the content of) the executable `path` holding
/// `body`, mode 0755, without this process opening `path` for writing.
pub fn write_executable(path: &Path, body: impl AsRef<[u8]>) {
    let staged = path.with_extension("src");
    assert_ne!(staged, path, "the program must not end in .src");
    fs::write(&staged, body).unwrap();
    let status = Command::new("/bin/cp")
        .arg(&staged)
        .arg(path)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "cp {} {}",
        staged.display(),
        path.display()
    );
    // The staged copy would show up as an untracked file in a work tree.
    fs::remove_file(&staged).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}
