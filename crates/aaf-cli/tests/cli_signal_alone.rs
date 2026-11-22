//! A signal sent to `aaf slot` or `aaf gate` alone, not to the terminal's
//! process group: `kill <pid>` from a supervisor, the factory or a person.
//! The heavy command runs in a process group of its own, so the signal
//! never reaches it; `aaf` died and the command ran on holding its slot.
//! Now the command's group is ended and the slot is free again. Real
//! processes through the built binary; the stand-in is a shell that writes
//! its pid and sleeps, no real build tool is started.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]

#[path = "cli_support.rs"]
mod support;

use std::fs;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use support::{Sandbox, calm_machine, memory_is_calm, write};

const PATIENCE: Duration = Duration::from_secs(30);

/// Time the command's group gets to be gone after the signal: the relay
/// forwards at once and the stand-in does not trap it, so this is far more
/// than needed and well below the relay's 10 s grace plus SIGKILL rounds.
const GONE_WITHIN: Duration = Duration::from_secs(8);

fn wait_for<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

/// Never panics: it also runs in [`Started`]'s drop, during an unwind.
fn kill(signal: &str, target: &str) -> bool {
    Command::new("/bin/kill")
        .args([signal, "--", target])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn group_alive(pgid: u32) -> bool {
    kill("-0", &format!("-{pgid}"))
}

/// SIGKILL to the group until it has no member left (one SIGKILL can miss
/// a member being forked at that instant on macOS).
fn end_group(pgid: u32) -> bool {
    let deadline = Instant::now() + PATIENCE;
    while group_alive(pgid) {
        if Instant::now() >= deadline {
            return false;
        }
        kill("-KILL", &format!("-{pgid}"));
        thread::sleep(Duration::from_millis(20));
    }
    true
}

fn shell_pid(pid_file: &Path) -> Option<u32> {
    let text = fs::read_to_string(pid_file).ok()?;
    text.trim().parse().ok().filter(|pid| *pid > 1)
}

/// True once a process of group `pgid` runs the relay's pipe reader: the
/// relay has checked whose command it guards and is waiting.
fn relay_ready(pgid: u32) -> bool {
    let out = Command::new("ps")
        .args(["-A", "-o", "pgid=,command="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).lines().any(|line| {
        let mut words = line.split_whitespace();
        words.next() == Some(pgid.to_string().as_str()) && line.contains("aaf-relay-reader")
    })
}

/// Ends what the test started, also when an assertion fails on the way:
/// the stand-in must never be left running on the developer's machine.
struct Started {
    aaf: Child,
    pid_file: PathBuf,
    /// Set once the test saw the group end: a free group id may belong to
    /// somebody else by the time this drops.
    ended: bool,
}

impl Drop for Started {
    fn drop(&mut self) {
        // Gone already in the passing case.
        let _ = self.aaf.kill();
        let _ = self.aaf.wait();
        // The relay too, if it is still there: it leads no group of its
        // own, it is in the one `aaf` leads.
        kill("-KILL", &format!("-{}", self.aaf.id()));
        if self.ended {
            return;
        }
        if let Some(pgid) = shell_pid(&self.pid_file) {
            end_group(pgid);
        }
    }
}

fn stand_in(pid_file: &Path) -> String {
    format!("echo $$ > '{}'; sleep 600", pid_file.display())
}

/// Sends `signal` to the `aaf` process alone and checks that its command's
/// group ends, that `aaf` itself ended by that signal, and that the slot is
/// free again.
fn signal_alone_ends_the_command(sb: &Sandbox, mut command: Command, signal: &str, number: i32) {
    let pid_file = sb.home.join("first.pid");
    command
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // Its own group, so the test can be sure nothing but the one pid
        // is signalled, and the relay can be found by its group.
        .process_group(0);
    let mut started = Started {
        aaf: command.spawn().unwrap(),
        pid_file: pid_file.clone(),
        ended: false,
    };
    let aaf_pid = started.aaf.id();
    let pgid = wait_for("the command to start", || shell_pid(&pid_file));
    wait_for("the signal relay", || relay_ready(aaf_pid).then_some(()));

    assert!(kill(&format!("-{signal}"), &aaf_pid.to_string()));
    let status = started.aaf.wait().unwrap();
    assert_eq!(status.signal(), Some(number), "{status:?}");

    let deadline = Instant::now() + GONE_WITHIN;
    while group_alive(pgid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    let survived = group_alive(pgid);
    // Whatever the outcome, no stray `sleep` survives the test.
    started.ended = end_group(pgid);
    assert!(
        !survived,
        "the command's group outlived the {signal} sent to aaf alone"
    );

    // The slot is free: with one slot on the machine the next command is
    // admitted. Memory pressure that rose since the test began is waited
    // for once the slot is given, as designed; that still shows it free.
    let mark = sb.home.join("second-ran");
    let out = sb.aaf(&[
        "slot",
        "--timeout",
        "30",
        "--",
        "touch",
        mark.to_str().unwrap(),
    ]);
    let said = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(!said.contains("waiting for a heavy slot"), "{said}");
    match out.status.code() {
        Some(0) => assert!(mark.exists()),
        code => {
            assert_eq!(code, Some(75), "{said}");
            assert!(said.contains("waiting for memory pressure"), "{said}");
        }
    }
}

fn slot_case(signal: &str, number: i32) {
    if !memory_is_calm("signal_alone_ends_the_command_of_aaf_slot") {
        return;
    }
    let sb = Sandbox::without_git();
    calm_machine(&sb, 1);
    let script = stand_in(&sb.home.join("first.pid"));
    let command = sb.command(&["slot", "--", "sh", "-c", &script]);
    signal_alone_ends_the_command(&sb, command, signal, number);
}

#[test]
fn a_terminate_sent_to_aaf_slot_alone_ends_its_command_and_frees_the_slot() {
    slot_case("TERM", 15);
}

#[test]
fn a_hangup_sent_to_aaf_slot_alone_ends_its_command_and_frees_the_slot() {
    slot_case("HUP", 1);
}

#[test]
fn a_terminate_sent_to_aaf_gate_alone_ends_its_command_and_frees_the_slot() {
    if !memory_is_calm("signal_alone_ends_the_command_of_aaf_gate") {
        return;
    }
    let sb = Sandbox::new();
    calm_machine(&sb, 1);
    write(
        &sb.repo.join(".autofactory/config.toml"),
        &format!(
            "[commands]\ntest_scoped = \"{}\"\n",
            stand_in(&sb.home.join("first.pid"))
        ),
    );
    let command = sb.command(&["gate", "--scoped", "--json"]);
    signal_alone_ends_the_command(&sb, command, "TERM", 15);
}
