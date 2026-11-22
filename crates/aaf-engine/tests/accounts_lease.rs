#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};

use aaf_engine::accounts::{AccountsError, Lease};

const HELPER_DIR_VAR: &str = "AAF_TEST_LEASE_HELPER_DIR";
const HELPER_ACCOUNT: &str = "claude_code-child";

#[test]
fn second_acquire_in_same_process_is_refused_and_drop_releases() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("leases");

    let first = Lease::try_acquire(&dir, "claude_code-abc")
        .unwrap()
        .unwrap();
    assert_eq!(first.path(), dir.join("claude_code-abc.lock"));
    assert!(
        Lease::try_acquire(&dir, "claude_code-abc")
            .unwrap()
            .is_none()
    );
    // Another account is independent.
    assert!(Lease::try_acquire(&dir, "codex-abc").unwrap().is_some());

    drop(first);
    assert!(
        Lease::try_acquire(&dir, "claude_code-abc")
            .unwrap()
            .is_some()
    );
}

#[test]
fn unsafe_account_ids_are_rejected_and_safe_ones_accepted() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("leases");
    for bad in ["", "../escape", "a/b", ".hidden", "a b", "a\0b"] {
        assert!(
            matches!(
                Lease::try_acquire(&dir, bad),
                Err(AccountsError::InvalidAccountId(_))
            ),
            "{bad:?}"
        );
    }
    assert!(!tmp.path().join("escape.lock").exists());
    assert!(
        Lease::try_acquire(&dir, "open_code-0a1b.slot1")
            .unwrap()
            .is_some()
    );
}

#[test]
fn the_lease_file_and_its_directory_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("leases");

    let lease = Lease::try_acquire(&dir, "claude_code-abc")
        .unwrap()
        .unwrap();

    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(lease.path()), 0o600);
}

#[test]
fn symlinked_lock_file_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("elsewhere");
    std::os::unix::fs::symlink(&target, tmp.path().join("acct.lock")).unwrap();

    assert!(matches!(
        Lease::try_acquire(tmp.path(), "acct"),
        Err(AccountsError::SymlinkLease { .. })
    ));
    assert!(!target.exists());
}

/// Not a test of its own: when re-executed by the test below with the helper
/// variable set, this process takes the lease, says so and waits for stdin
/// to close. Ignored so that a plain `cargo test` does not count a body that
/// asserts nothing as a passing test; the parent runs it with `--ignored`.
#[test]
#[ignore = "helper process of lease_held_by_another_process_is_seen_and_freed_when_it_is_killed"]
fn child_helper_holds_lease() {
    let Some(dir) = std::env::var_os(HELPER_DIR_VAR) else {
        return;
    };
    let lease = Lease::try_acquire(Path::new(&dir), HELPER_ACCOUNT).unwrap();
    println!("{}", if lease.is_some() { "ready" } else { "refused" });
    let mut sink = Vec::new();
    std::io::stdin().read_to_end(&mut sink).unwrap();
}

fn spawn_helper(dir: &Path) -> (Child, BufReader<ChildStdout>, String) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "child_helper_holds_lease",
            "--nocapture",
        ])
        .env(HELPER_DIR_VAR, dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // The answer line is the synchronisation point; EOF (child died) ends
    // the loop, so this cannot hang on a broken helper.
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let answer = stdout
        .by_ref()
        .lines()
        .map_while(Result::ok)
        .find(|line| line == "ready" || line == "refused")
        .unwrap_or_default();
    // The pipe stays open so the harness in the child can still print its
    // summary; closing it early would make a clean exit fail on EPIPE.
    (child, stdout, answer)
}

#[test]
fn lease_held_by_another_process_is_seen_and_freed_when_it_is_killed() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    let (mut child, _stdout, answer) = spawn_helper(dir);
    assert_eq!(answer, "ready");
    assert!(Lease::try_acquire(dir, HELPER_ACCOUNT).unwrap().is_none());

    // SIGKILL: no destructor runs in the child, the kernel frees the lock.
    child.kill().unwrap();
    child.wait().unwrap();
    let mine = Lease::try_acquire(dir, HELPER_ACCOUNT).unwrap();
    assert!(mine.is_some());

    // And the other direction: a child cannot take what this process holds.
    let (mut child, _stdout, answer) = spawn_helper(dir);
    assert_eq!(answer, "refused");
    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
}
