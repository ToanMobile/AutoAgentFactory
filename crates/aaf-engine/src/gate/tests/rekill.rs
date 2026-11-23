//! A timed-out command's process group is empty when the gate returns.
//!
//! One SIGKILL to a group misses a member that is being forked at that
//! instant on macOS: the child is created after the signal was delivered,
//! keeps the group id and runs on with ppid 1 (measured: most single kills
//! of a shell that forks in a loop leave one). The gate repeats the kill
//! until `kill -0` finds the group empty.
//!
//! Probabilistic on the old code: each attempt may or may not catch a fork
//! in flight, so the test makes up to `ATTEMPTS` of them and fails on the
//! first survivor. With the repeated kill no attempt may leave one.

use std::fs;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

use super::super::proc::capture;

const ATTEMPTS: usize = 12;

/// True while process group `pgid` still has a live (non-zombie) member.
/// On Linux, killed orphaned children briefly remain as zombies until PID 1
/// reaps them, and bare `kill -0 -- -<pgid>` still reports success for them.
fn group_alive(pgid: u32) -> bool {
    let exists = Command::new("/bin/kill")
        .args(["-0", "--", &format!("-{pgid}")])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success();
    if !exists {
        return false;
    }
    let Ok(out) = Command::new("/bin/ps")
        .args(["-A", "-o", "pgid=,stat="])
        .stderr(Stdio::null())
        .output()
    else {
        return true;
    };
    if !out.status.success() {
        return true;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    if text.trim().is_empty() {
        return true;
    }
    let target = pgid.to_string();
    text.lines().any(|line| {
        let mut cols = line.split_whitespace();
        let Some(g) = cols.next() else {
            return false;
        };
        let Some(stat) = cols.next() else {
            return false;
        };
        g == target && !stat.starts_with('Z')
    })
}

/// Ends whatever the attempt left behind, so no `sleep 600` outlives the
/// test even when it fails.
struct Reaper(Option<u32>);

impl Drop for Reaper {
    fn drop(&mut self) {
        let Some(pgid) = self.0 else { return };
        let end = Instant::now() + Duration::from_secs(10);
        while group_alive(pgid) && Instant::now() < end {
            let _ = Command::new("/bin/kill")
                .args(["-KILL", "--", &format!("-{pgid}")])
                .stderr(Stdio::null())
                .status();
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[test]
fn a_timed_out_command_that_keeps_forking_leaves_no_member_of_its_group() {
    let dir = TempDir::new().unwrap();
    let pid_file = dir.path().join("pgid");
    for attempt in 0..ATTEMPTS {
        let _ = fs::remove_file(&pid_file);
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(format!(
            "echo $$ > '{}'; while :; do /bin/sleep 600 >/dev/null 2>&1 & done",
            pid_file.display()
        ));
        let done = capture(cmd, Duration::from_millis(500), Some(1024), true).unwrap();
        assert!(
            done.timed_out,
            "attempt {attempt}: the loop ended by itself"
        );
        // On a very busy machine the shell may not have started before the
        // kill: nothing was forked, and the attempt proves nothing.
        let Ok(text) = fs::read_to_string(&pid_file) else {
            continue;
        };
        let pgid: u32 = text.trim().parse().unwrap();
        let _guard = Reaper(Some(pgid));
        assert!(
            !group_alive(pgid),
            "attempt {attempt}: process group {pgid} still has a member after the timeout kill"
        );
    }
}
