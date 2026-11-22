//! An `aaf` killed together with its signal relay leaves its heavy command
//! running: the command lives in its own process group, and without
//! `unsafe` there is no signal handler to take it down. The kernel frees the
//! slot lock at that moment, so the next heavy command must find out by
//! itself that the slot is still in use. Real processes through the built
//! binary, for `aaf slot` and `aaf gate`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "cli_support.rs"]
mod support;

use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use support::{Sandbox, calm_machine, memory_is_calm, write};

const PATIENCE: Duration = Duration::from_secs(30);

/// Polls until `probe` gives a value; panics with `what` after [`PATIENCE`].
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

/// SIGKILL to the group until it has no member left; false if it still has
/// one after [`PATIENCE`]. One signal is not enough: a member being forked
/// at that instant is not signalled, and the child, reparented to pid 1,
/// runs on in the group (measured on macOS 27 under load: a shell forking
/// `sleep 600` in a loop kept one such child after a single group SIGKILL
/// in 22 of 60 runs). That is how a `sleep 600` outlived this test once.
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

/// The pid the command's shell wrote, once it is there.
fn shell_pid(pid_file: &Path) -> Option<u32> {
    let text = fs::read_to_string(pid_file).ok()?;
    text.trim().parse().ok().filter(|pid| *pid > 1)
}

/// The process group of a running process, as `ps` reports it.
fn group_of(pid: u32) -> Option<u32> {
    let out = Command::new("ps")
        .args(["-o", "pgid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Ends what the test started, also when an assertion fails on the way: the
/// stand-in must never be left running on the developer's machine.
struct Started {
    aaf: Child,
    /// Where the command's shell writes its pid; it leads the command's
    /// process group.
    pid_file: PathBuf,
    /// Set once the test itself saw the group end: a group id that is free
    /// again may belong to somebody else by the time this drops.
    ended: bool,
}

impl Drop for Started {
    fn drop(&mut self) {
        // Gone already in the passing case.
        let _ = self.aaf.kill();
        let _ = self.aaf.wait();
        if self.ended {
            return;
        }
        if let Some(pgid) = shell_pid(&self.pid_file) {
            end_group(pgid);
        }
    }
}

/// The command both tests leave behind: writes its pid, then sleeps.
fn stand_in(pid_file: &Path) -> String {
    format!("echo $$ > '{}'; sleep 600", pid_file.display())
}

/// The process group in the record of a slot's holder, once the record is
/// whole. The holder writes `pgid=` at once and `lstart=` one `ps` run
/// later; a holder killed in between leaves a record the next command does
/// not take for an orphan, and that is not the situation tested here.
fn complete_record(text: &str) -> Option<u32> {
    let value = |key: &str| text.lines().find_map(|line| line.strip_prefix(key));
    value("lstart=").filter(|started| !started.is_empty())?;
    value("pgid=")?.parse().ok()
}

/// The process group the holder of slot 0 recorded for its command.
fn recorded_group(sb: &Sandbox) -> Option<u32> {
    complete_record(&fs::read_to_string(sb.data.join("throttle/heavy.0.lock")).ok()?)
}

#[test]
fn record_is_complete_only_with_the_start_time_of_the_group_leader() {
    let early = "reserved=unknown\npgid=4242\n";
    assert_eq!(complete_record(early), None);
    let full = format!("{early}lstart=Sun Nov 22 09:12:08 2026\n");
    assert_eq!(complete_record(&full), Some(4242));
    assert_eq!(complete_record(&format!("{early}lstart=\n")), None);
}

fn group_alive(pgid: u32) -> bool {
    kill("-0", &format!("-{pgid}"))
}

/// `aaf slot --timeout <secs> -- touch <mark>`: exit code (0 when admitted,
/// 75 when not) and what `aaf` said while it waited.
fn next_heavy_command(sb: &Sandbox, mark: &Path, secs: &str) -> (i32, String) {
    let mark = mark.to_str().unwrap();
    let out = sb.aaf(&["slot", "--timeout", secs, "--", "touch", mark]);
    let said = String::from_utf8_lossy(&out.stderr).into_owned();
    (out.status.code().unwrap(), said)
}

/// Kills `aaf` and its signal relay at once with SIGKILL (its whole process
/// group, as the factory's timeout kill of a gate does), so nothing is left
/// to stop the command, then checks the queue around the command it left
/// behind. A signal to `aaf` alone, even SIGKILL, now ends the command
/// through the relay (`cli_signal_alone.rs`). `aaf` must lead its own
/// group, or the test runner would be killed with it.
fn orphan_keeps_the_slot(sb: &Sandbox, aaf: Child, pid_file: &Path) {
    let mut started = Started {
        aaf,
        pid_file: pid_file.to_path_buf(),
        ended: false,
    };
    let shell = wait_for("the command to start", || shell_pid(pid_file));
    let pgid = wait_for("the command's group in the lock file", || {
        recorded_group(sb)
    });
    // The lock names the group the command really runs in, not the one of
    // `aaf` and its signal relay.
    assert_eq!(
        Some(pgid),
        group_of(shell),
        "recorded group vs the command's"
    );

    let aaf_group = started.aaf.id();
    assert_eq!(group_of(aaf_group), Some(aaf_group), "aaf leads its group");
    assert!(kill("-KILL", &format!("-{aaf_group}")));
    started.aaf.wait().unwrap();
    // This is the situation: `aaf` is gone and its command runs on.
    assert!(group_alive(pgid), "the command died with aaf");

    let mark = sb.home.join("second-ran");
    let (code, said) = next_heavy_command(sb, &mark, "1");
    let overlapped = mark.exists();
    // Whatever the outcome, no stray `sleep` survives the test.
    started.ended = end_group(pgid);
    assert!(started.ended, "the orphan outlived repeated SIGKILLs");
    assert!(
        !overlapped,
        "a second heavy command ran beside the orphan of a killed aaf"
    );
    assert_eq!(
        code, 75,
        "the wait for the slot should have run out: {said}"
    );

    // The orphan is gone: the slot is free again, with no stale state.
    // Memory pressure that rose since the test began is waited for only
    // once the slot is given (`admit_ask`), as designed; such a wait can
    // outlast any limit on a busy machine, and still shows the slot free.
    let (code, said) = next_heavy_command(sb, &mark, "30");
    assert!(!said.contains("waiting for a heavy slot"), "{said}");
    if code == 0 {
        assert!(mark.exists());
    } else {
        assert_eq!(code, 75, "{said}");
        assert!(said.contains("waiting for memory pressure"), "{said}");
    }
}

#[test]
fn command_left_by_a_killed_aaf_slot_keeps_its_slot_until_it_ends() {
    if !memory_is_calm("command_left_by_a_killed_aaf_slot") {
        return;
    }
    let sb = Sandbox::without_git();
    calm_machine(&sb, 1);
    let pid_file = sb.home.join("first.pid");
    let aaf = sb
        .command(&["slot", "--", "sh", "-c", &stand_in(&pid_file)])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    orphan_keeps_the_slot(&sb, aaf, &pid_file);
}

#[test]
fn command_left_by_a_killed_aaf_gate_keeps_its_slot_until_it_ends() {
    if !memory_is_calm("command_left_by_a_killed_aaf_gate") {
        return;
    }
    let sb = Sandbox::new();
    calm_machine(&sb, 1);
    let pid_file = sb.home.join("first.pid");
    write(
        &sb.repo.join(".autofactory/config.toml"),
        &format!("[commands]\ntest_scoped = \"{}\"\n", stand_in(&pid_file)),
    );
    let aaf = sb
        .command(&["gate", "--scoped", "--json"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    orphan_keeps_the_slot(&sb, aaf, &pid_file);
}
