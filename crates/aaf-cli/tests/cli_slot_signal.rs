//! Ctrl-C on `aaf slot -- <command>` (as in a git hook that runs
//! `aaf slot -- ./gradlew …`). The command runs in a process group of its
//! own, so the terminal's interrupt reaches `aaf` and never the command:
//! `aaf` ended and the build ran on. The stand-in is a shell that traps
//! nothing and sleeps; no real build tool is started.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]

#[path = "cli_support.rs"]
mod support;

use std::fs;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use support::{Sandbox, calm_machine, memory_is_calm};

const PATIENCE: Duration = Duration::from_secs(60);

fn ps(args: &[&str]) -> String {
    let out = Command::new("ps").args(args).output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn alive(pid: &str) -> bool {
    !ps(&["-o", "pid=", "-p", pid]).trim().is_empty()
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Ends what the test started, also when an assertion fails on the way: the
/// stand-in must never be left running on the developer's machine.
struct Started {
    aaf: Child,
    pid_file: PathBuf,
}

impl Drop for Started {
    fn drop(&mut self) {
        // Both may be gone already; that is the passing case.
        let _ = self.aaf.kill();
        let _ = self.aaf.wait();
        let pid = fs::read_to_string(&self.pid_file).unwrap_or_default();
        // The command is the leader of its own process group.
        if pid.trim().parse::<u32>().is_ok_and(|pid| pid > 1) {
            let _ = Command::new("/bin/kill")
                .args(["-KILL", "--", &format!("-{}", pid.trim())])
                .stderr(Stdio::null())
                .status();
        }
    }
}

/// `aaf slot -- sh -c 'echo $$ > pid; sleep 300'` as the leader of a new
/// process group, which stands for the terminal's foreground group: the
/// signal goes to that group, as a Ctrl-C does.
fn interrupted_slot_ends_its_command(signal: &str, number: i32) {
    if !memory_is_calm("interrupted_slot_ends_its_command") {
        return;
    }
    let sb = Sandbox::new();
    calm_machine(&sb, 1);
    let pid_file = sb.home.join("command.pid");
    let script = format!("echo $$ > '{}'; sleep 300", pid_file.display());
    let mut command = sb.command(&["slot", "--", "sh", "-c", &script]);
    command
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    let mut started = Started {
        aaf: command.spawn().unwrap(),
        pid_file: pid_file.clone(),
    };
    let aaf = &mut started.aaf;
    let group = aaf.id().to_string();

    wait_until("the command to start", || {
        fs::read_to_string(&pid_file).is_ok_and(|text| !text.trim().is_empty())
    });
    let command_pid = fs::read_to_string(&pid_file).unwrap().trim().to_string();
    // The relay is started on the watchdog's first look at the command; a
    // signal before that finds nobody to pass it on. Its name is in the
    // process table from its exec on, before its traps are set: a signal
    // then ends the relay itself and nothing is passed on (17 of 20 tries
    // with the signal sent at once). Its reader starts after the traps, so
    // the reader is what says the relay is ready.
    wait_until("the signal relay", || {
        ps(&["-A", "-o", "pgid=,command="]).lines().any(|line| {
            let mut words = line.split_whitespace();
            words.next() == Some(group.as_str()) && line.contains("aaf-relay-reader")
        })
    });

    let sent = Command::new("/bin/kill")
        .args([&format!("-{signal}"), "--", &format!("-{group}")])
        .status()
        .unwrap();
    assert!(sent.success());

    let status = aaf.wait().unwrap();
    let deadline = Instant::now() + PATIENCE;
    while alive(&command_pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !alive(&command_pid),
        "the command outlived the interrupted aaf slot"
    );
    // Ended by the signal itself: a shell reports that as 128 + signal.
    assert_eq!(status.signal(), Some(number), "{status:?}");

    // The slot is free again: with one slot on the machine the next
    // command is admitted at once.
    let next = sb.aaf(&["slot", "--timeout", "30", "--", "true"]);
    assert_eq!(
        next.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&next.stderr)
    );
}

#[test]
fn an_interrupt_to_the_foreground_group_ends_the_command_of_aaf_slot() {
    interrupted_slot_ends_its_command("INT", 2);
}

#[test]
fn a_terminate_to_the_foreground_group_ends_the_command_of_aaf_slot() {
    interrupted_slot_ends_its_command("TERM", 15);
}
