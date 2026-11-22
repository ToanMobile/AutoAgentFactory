//! The signal relay leaves the command's process group empty.
//!
//! One SIGKILL to a group misses a member that is being forked at that
//! instant on macOS: the child is created after the signal was delivered,
//! keeps the group id and runs on (measured: 21 of 30 and 22 of 60 single
//! kills of a shell that forks in a loop left one). The relay repeats the
//! kill until `kill -0` finds the group empty, as the gate's timeout kill
//! does (`gate/tests/rekill.rs`).
//!
//! Probabilistic on the old code: each attempt may or may not catch a fork
//! in flight, so the test makes up to `ATTEMPTS` of them and fails on the
//! first survivor. With the repeated kill no attempt may leave one.

use std::fs;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

use super::super::relay::SignalRelay;

const ATTEMPTS: usize = 12;
/// Bound on every wait below; reached only when something is broken.
const LIMIT: Duration = Duration::from_secs(60);

/// True also when `kill` cannot be started (the process table is full):
/// the [`Reaper`] then keeps trying, where a panic in its `drop` during an
/// unwind aborted the test binary and left the forking group running.
fn group_alive(pgid: u32) -> bool {
    Command::new("/bin/kill")
        .args(["-0", "--", &format!("-{pgid}")])
        .stderr(Stdio::null())
        .status()
        .map_or(true, |status| status.success())
}

/// Ends whatever the attempt left behind, so no `sleep 600` outlives the
/// test even when it fails.
struct Reaper(u32);

impl Drop for Reaper {
    fn drop(&mut self) {
        let end = Instant::now() + Duration::from_secs(10);
        while group_alive(self.0) && Instant::now() < end {
            let _ = Command::new("/bin/kill")
                .args(["-KILL", "--", &format!("-{}", self.0)])
                .stderr(Stdio::null())
                .status();
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Polls `ready` until it holds; false after [`LIMIT`].
fn wait_for(ready: &dyn Fn() -> bool) -> bool {
    let end = Instant::now() + LIMIT;
    while Instant::now() < end {
        if ready() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

/// The relay has set its traps once it has started its `cat`.
fn has_child(pid: u32) -> bool {
    let out = Command::new("/bin/ps")
        .args(["-A", "-o", "ppid="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|line| line.trim().parse() == Ok(pid))
}

/// A command that ignores the forwarded signal and keeps forking, so the
/// relay's SIGKILL lands while a fork is in flight. Each child lives one
/// second: with `sleep 600` a kill delayed under load let the loop fill
/// the user's process table (3570 of 4000 seen, every spawn of every test
/// then failed with EAGAIN, and the reaper's panic aborted the binary).
fn forking_group(pid_file: &Path) -> std::process::Child {
    let _ = fs::remove_file(pid_file);
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(format!(
            "trap '' INT TERM HUP; echo $$ > '{}'; \
             while :; do /bin/sleep 1 >/dev/null 2>&1 & done",
            pid_file.display()
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    cmd.spawn().unwrap()
}

#[test]
fn a_relayed_interrupt_leaves_no_member_of_a_command_that_keeps_forking() {
    let dir = TempDir::new().unwrap();
    let pid_file = dir.path().join("pgid");
    for attempt in 0..ATTEMPTS {
        let mut child = forking_group(&pid_file);
        let pgid = child.id();
        let _reaper = Reaper(pgid);
        assert!(
            wait_for(&|| pid_file.is_file()),
            "attempt {attempt}: the command never started"
        );
        // No grace: the forwarded TERM is ignored, the SIGKILL comes at once.
        let mut relay = SignalRelay::start_with_grace(pgid, 0).unwrap();
        let relay_pid = relay.id();
        assert!(
            wait_for(&|| has_child(relay_pid)),
            "attempt {attempt}: the relay never got ready"
        );
        let sent = Command::new("/bin/kill")
            .args(["-TERM", &relay_pid.to_string()])
            .status()
            .unwrap();
        assert!(sent.success(), "attempt {attempt}: no signal to the relay");
        assert!(
            relay.ended_within(LIMIT),
            "attempt {attempt}: the relay did not finish"
        );
        let alive = group_alive(pgid);
        // The leader was killed; collect it before the next attempt.
        let _ = child.wait();
        assert!(
            !alive,
            "attempt {attempt}: process group {pgid} still has a member after the relay's kill"
        );
        relay.stop();
    }
}

/// True once `relay` runs its pipe reader: it has checked whose group it
/// guards and is waiting.
fn reader_started(relay: u32) -> bool {
    let out = Command::new("/bin/ps")
        .args(["-A", "-o", "ppid=,command="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).lines().any(|line| {
        let mut words = line.split_whitespace();
        words.next().and_then(|ppid| ppid.parse().ok()) == Some(relay)
            && line.contains("aaf-relay-reader")
    })
}

/// True once `pid` has exited (gone, or a zombie nobody collected yet).
fn exited(pid: u32) -> bool {
    let out = Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout);
    stat.trim().is_empty() || stat.trim().starts_with('Z')
}

/// `sleep 600` leading a process group of its own, a child of this process
/// as a heavy command is a child of `aaf`.
fn sleeping_group() -> std::process::Child {
    Command::new("/bin/sleep")
        .arg("600")
        .stdin(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap()
}

/// A relay whose owner ends without saying the command is over (what the
/// death of `aaf` by a signal sent to it alone looks like) ends the group.
#[test]
fn a_relay_whose_owner_ends_without_saying_so_ends_the_group() {
    let mut child = sleeping_group();
    let pgid = child.id();
    let _reaper = Reaper(pgid);
    let mut relay = SignalRelay::start(pgid).unwrap();
    assert!(wait_for(&|| reader_started(relay.id())), "relay not ready");

    relay.abandon();

    assert!(relay.ended_within(LIMIT), "the relay did not finish");
    let _ = child.wait();
    assert!(!group_alive(pgid), "process group {pgid} outlived its aaf");
}

/// The allowed neighbour: told that the command is over, the relay ends
/// and leaves the group alone, also when it is dropped instead of stopped.
#[test]
fn a_relay_told_the_command_is_over_leaves_the_group_alone() {
    for drop_it in [false, true] {
        let mut child = sleeping_group();
        let pgid = child.id();
        let _reaper = Reaper(pgid);
        let relay = SignalRelay::start(pgid).unwrap();
        let relay_pid = relay.id();
        assert!(wait_for(&|| reader_started(relay_pid)), "relay not ready");

        if drop_it {
            drop(relay);
        } else {
            relay.stop();
        }
        // A relay that decided to stop the group signals it before it
        // exits, and while the group is there it does not exit.
        assert!(wait_for(&|| exited(relay_pid)), "the relay did not finish");
        assert!(
            matches!(child.try_wait(), Ok(None)),
            "the command was stopped although it was not over (drop={drop_it})"
        );
    }
}

/// A group whose leader is not a child of the relay's owner (a made-up id,
/// an id that went to somebody else) is never signalled because the pipe
/// closed.
#[test]
fn a_relay_for_a_group_its_owner_did_not_start_never_ends_it_unasked() {
    let dir = TempDir::new().unwrap();
    let pid_file = dir.path().join("pgid");
    // `set -m` puts the background `sleep` in a group of its own, led by
    // a grandchild of this process.
    let mut outer = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!(
            "set -m; /bin/sleep 600 & echo $! > '{}'; wait",
            pid_file.display()
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let written =
        || fs::read_to_string(&pid_file).is_ok_and(|text| text.trim().parse::<u32>().is_ok());
    assert!(wait_for(&written), "the group never started");
    let pgid: u32 = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let reaper = Reaper(pgid);
    let mut relay = SignalRelay::start(pgid).unwrap();
    assert!(wait_for(&|| reader_started(relay.id())), "relay not ready");

    relay.abandon();

    assert!(relay.ended_within(LIMIT), "the relay did not finish");
    let alive = group_alive(pgid);
    drop(reaper);
    let _ = outer.wait();
    assert!(alive, "a group this process did not start was signalled");
}

/// The relay notices the death of its owner while the pipe is still open:
/// a copy of its writing end that leaked into a child another thread of
/// `aaf` spawned keeps it open after `aaf` died (the write end has no
/// atomic close-on-exec on macOS, see `aaf_fp::output`). Here this test
/// holds that copy, and the owner is a shell standing in for `aaf` whose
/// child leads the command's group.
#[test]
fn a_relay_ends_the_group_when_its_owner_dies_while_the_pipe_is_still_held() {
    let dir = TempDir::new().unwrap();
    let pid_file = dir.path().join("pgid");
    let mut owner = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!(
            "set -m; /bin/sleep 600 & echo $! > '{}'; wait",
            pid_file.display()
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let written =
        || fs::read_to_string(&pid_file).is_ok_and(|text| text.trim().parse::<u32>().is_ok());
    assert!(wait_for(&written), "the group never started");
    let pgid: u32 = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let _reaper = Reaper(pgid);
    let mut relay = SignalRelay::start_for(pgid, 0, owner.id()).unwrap();
    assert!(wait_for(&|| reader_started(relay.id())), "relay not ready");

    // The owner dies; its parent collects it at once, as the shell or
    // process that ran `aaf` does.
    owner.kill().unwrap();
    owner.wait().unwrap();

    assert!(
        relay.ended_within(Duration::from_secs(30)),
        "the relay waited for the pipe although its owner is gone"
    );
    assert!(
        !group_alive(pgid),
        "process group {pgid} outlived its owner"
    );
}
