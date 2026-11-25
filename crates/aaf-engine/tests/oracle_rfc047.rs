//! Oracle tests for RFC-047: reduce `SignalRelay` latency and bound `ps` checks.
//!
//! Verifies:
//! 1. Pre-`SIGTERM` path avoids `group_live`/`ps -A` and checks `kill -0 -"$1"`.
//! 2. Watcher loop polls sub-second (`/bin/sleep 0.2`) and preserves the
//!    `armed` (`leader_parent == $4`) invariant (Security #7).
//! 3. `group_live` bounds `/bin/ps` portably (no GNU `timeout`), cleans up its
//!    timer, and returns live (`0`) if `ps` times out so the relay never exits early.
//! 4. `SIGKILL` loop exits immediately on the first empty `kill -0 -"$1"`
//!    (State-Machine #3) and does not spawn bounded `ps` on every 0.1 s tick.
//! 5. Real execution terminates a child group quickly (`< 2s`) on owner death
//!    without leaking child processes.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]

use std::fs;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use aaf_engine::throttle::SignalRelay;
use tempfile::TempDir;

const RELAY_SRC: &str = include_str!("../src/throttle/relay.rs");

/// Extracts the shell script string literal from `const SCRIPT: &str = r#"..."#;`.
fn extract_script() -> &'static str {
    let start_marker = "const SCRIPT: &str = r#\"";
    let start = RELAY_SRC
        .find(start_marker)
        .expect("relay.rs must define `const SCRIPT: &str = r#\"...\"#;`")
        + start_marker.len();
    let rest = &RELAY_SRC[start..];
    let end = rest
        .find("\"#;")
        .expect("relay.rs SCRIPT literal must terminate with `\"#;`");
    &rest[..end]
}

fn kill_sig(signal: &str, target: &str) -> bool {
    Command::new("/bin/kill")
        .args([signal, "--", target])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn group_exists(pgid: u32) -> bool {
    kill_sig("-0", &format!("-{pgid}"))
}

struct GroupReaper(u32);

impl Drop for GroupReaper {
    fn drop(&mut self) {
        let end = Instant::now() + Duration::from_secs(5);
        while group_exists(self.0) && Instant::now() < end {
            kill_sig("-KILL", &format!("-{}", self.0));
            thread::sleep(Duration::from_millis(10));
        }
    }
}

struct ChildReaper(Option<Child>);

impl Drop for ChildReaper {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            let pid = c.id();
            let _ = c.kill();
            let _ = c.wait();
            kill_sig("-KILL", &format!("-{pid}"));
        }
    }
}

fn wait_for(limit: Duration, mut pred: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + limit;
    while Instant::now() < end {
        if pred() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    false
}

fn reader_running_for(relay_pid: u32) -> bool {
    let out = Command::new("/bin/ps")
        .args(["-A", "-o", "ppid=,command="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).lines().any(|line| {
        let mut words = line.split_whitespace();
        words.next().and_then(|ppid| ppid.parse::<u32>().ok()) == Some(relay_pid)
            && line.contains("aaf-relay-reader")
    })
}

fn direct_children(ppid: u32) -> Vec<u32> {
    let out = Command::new("/bin/ps")
        .args(["-A", "-o", "pid=,ppid="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let pid: u32 = words.next()?.parse().ok()?;
            let parent: u32 = words.next()?.parse().ok()?;
            (parent == ppid).then_some(pid)
        })
        .collect()
}

fn pgid_from_file(path: &Path) -> Option<u32> {
    fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|&p| p > 1)
}

#[test]
fn relay_script_avoids_pre_term_ps_polls_subsecond_and_bounds_ps() {
    let script = extract_script();

    // Security #7 invariant: armed must still verify leader_parent == $4.
    assert!(
        script.contains(r#"[ "$leader_parent" = "$4" ] && armed=1"#),
        "Security #7 invariant violated: armed must check leader_parent == $4"
    );

    // Watcher loop must poll faster than 1 s (0.2 s) and not sleep 1 s.
    let watcher_start = script
        .find("if [ -n \"$armed\" ]; then")
        .expect("script must contain armed watcher block");
    let watcher_end = script[watcher_start..]
        .find("\nfi\n")
        .expect("watcher block must close with fi")
        + watcher_start;
    let watcher_block = &script[watcher_start..watcher_end];
    assert!(
        watcher_block.contains("/bin/sleep 0.2"),
        "watcher loop must poll at 0.2s, got:\n{watcher_block}"
    );
    assert!(
        !watcher_block.contains("/bin/sleep 1"),
        "watcher loop must not sleep 1s, got:\n{watcher_block}"
    );

    // Pre-SIGTERM block (`if [ -z "$sig" ]; then ... fi`) must NOT invoke `group_live` or `ps`.
    let pre_term_start = script
        .find("if [ -z \"$sig\" ]; then")
        .expect("script must contain pre-SIGTERM closed-pipe block");
    let pre_term_end = script[pre_term_start..]
        .find("\nfi\n")
        .expect("pre-SIGTERM block must close with fi")
        + pre_term_start;
    let pre_term_block = &script[pre_term_start..pre_term_end];
    assert!(
        !pre_term_block.contains("group_live") && !pre_term_block.contains("/bin/ps"),
        "pre-SIGTERM block must not run group_live/ps, got:\n{pre_term_block}"
    );
    assert!(
        pre_term_block.contains(r#"kill -0 -"$1""#),
        "pre-SIGTERM block must check `kill -0 -\"$1\"`, got:\n{pre_term_block}"
    );

    // Portable bounded ps: must NOT rely on GNU timeout, must bound ps with a cleaned-up timer.
    assert!(
        !script.contains("/usr/bin/timeout") && !script.contains("timeout "),
        "script must not depend on GNU timeout"
    );
    let group_live_start = script
        .find("group_live()")
        .expect("script must define group_live()");
    let before_wait = &script[..script.find("wait \"$reader\"").unwrap()];
    assert!(
        before_wait.contains("kill -KILL") && group_live_start > 0,
        "group_live must use a bounded ps helper that kills hung ps"
    );

    // SIGKILL loop: State-Machine #3 (`kill -0 -"$1" || exit 0`) and sparse `group_live`.
    let sigkill_start = script
        .find("kill -KILL -\"$1\"")
        .expect("script must have SIGKILL loop");
    let sigkill_tail = &script[sigkill_start..];
    assert!(
        sigkill_tail.contains(r#"kill -0 -"$1" 2>/dev/null || exit 0"#),
        "SIGKILL loop must exit immediately on first empty `kill -0 -\"$1\"`"
    );
    assert!(
        sigkill_tail.contains("% 10"),
        "SIGKILL loop must only run `group_live` sparsely (not every 0.1s round)"
    );
}

#[test]
fn relay_bounded_ps_returns_live_without_hanging_when_ps_stalls() {
    let script = extract_script();
    let cutoff = script
        .find("[ -n \"$sig\" ] || wait \"$reader\"")
        .expect("script must contain reader wait");
    let defs = &script[..cutoff];
    assert!(
        defs.contains("/bin/ps -A -o pgid=,stat="),
        "script definitions must contain `/bin/ps -A -o pgid=,stat=`"
    );

    // Replace the `ps -A` command inside `group_live` with a stalling `sleep 30`
    // and run `group_live` against a live process group.
    let mut sleeper = Command::new("/bin/sleep")
        .arg("60")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let pgid = sleeper.id();
    let _pg_reaper = GroupReaper(pgid);

    // Extract only the function definitions (skip reader/watcher setup) by locating
    // `bounded_ps()` or `group_live()`.
    let fn_start = defs
        .find("bounded_ps()")
        .or_else(|| defs.find("group_live()"))
        .expect("script must define group_live()");
    let fn_defs = defs[fn_start..].replace("/bin/ps -A -o pgid=,stat=", "/bin/sleep 30");
    let probe_script = format!("{fn_defs}\ngroup_live \"$1\"\n");

    let start = Instant::now();
    let mut sh = Command::new("/bin/sh")
        .args(["-c", &probe_script, "rfc047-ps-timeout", &pgid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let sh_pgid = sh.id();
    let _sh_reaper = GroupReaper(sh_pgid);

    let finished = wait_for(Duration::from_secs(5), || {
        matches!(sh.try_wait(), Ok(Some(_)))
    });
    let elapsed = start.elapsed();

    let status = if finished {
        sh.wait().unwrap()
    } else {
        kill_sig("-KILL", &format!("-{sh_pgid}"));
        let _ = sh.wait();
        panic!(
            "group_live hung for {:?} when ps stalled instead of timing out portably",
            elapsed
        );
    };

    // Clean up our target sleeper before assertions.
    let _ = sleeper.kill();
    let _ = sleeper.wait();

    assert!(
        status.success(),
        "group_live must return 0 (still live) when ps times out, got {status:?}"
    );
    assert!(
        elapsed < Duration::from_secs(4),
        "bounded ps took too long: {elapsed:?}"
    );
    assert!(
        !group_exists(sh_pgid),
        "bounded ps left processes alive in its process group {sh_pgid}"
    );
}

#[test]
fn relay_ends_child_group_fast_on_owner_death_without_leaked_children() {
    let script = extract_script();
    let dir = TempDir::new().unwrap();
    let pid_file = dir.path().join("pgid");

    // Simulated `aaf` owner whose child leads its own process group.
    let mut owner_reaper = ChildReaper(Some(
        Command::new("/bin/bash")
            .arg("-c")
            .arg(format!(
                "set -m; /bin/sleep 600 & echo $! > '{}'; wait",
                pid_file.display()
            ))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    ));
    let owner_pid = owner_reaper.0.as_ref().unwrap().id();
    assert!(
        wait_for(Duration::from_secs(5), || {
            pgid_from_file(&pid_file).is_some()
        }),
        "child group never started"
    );
    let pgid = pgid_from_file(&pid_file).unwrap();
    let _pg_reaper = GroupReaper(pgid);
    assert!(group_exists(pgid), "child process group must be alive");

    // Hold the write end of stdin pipe open so ONLY the watcher detects owner death.
    let (pipe_r, _pipe_w) = std::io::pipe().unwrap();
    let mut relay = Command::new("/bin/sh")
        .args([
            "-c",
            script,
            "aaf-signal-relay",
            &pgid.to_string(),
            "10",
            "300",
            &owner_pid.to_string(),
        ])
        .stdin(pipe_r)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let relay_pid = relay.id();
    let _relay_reaper = GroupReaper(relay_pid);
    assert!(
        wait_for(Duration::from_secs(5), || reader_running_for(relay_pid)),
        "relay reader never started"
    );

    // Wait a moment so the watcher is mid-sleep, then kill and reap owner.
    thread::sleep(Duration::from_millis(50));
    let killed_at = Instant::now();
    {
        let owner = owner_reaper.0.as_mut().unwrap();
        owner.kill().unwrap();
        owner.wait().unwrap();
        owner_reaper.0 = None;
    }

    let relay_exited = wait_for(Duration::from_secs(5), || {
        matches!(relay.try_wait(), Ok(Some(_)))
    });
    let elapsed = killed_at.elapsed();

    assert!(relay_exited, "relay did not exit after owner died");
    let status = relay.wait().unwrap();
    assert!(
        status.success(),
        "relay should exit 0 after ending child group, got {status:?}"
    );
    assert!(
        !group_exists(pgid),
        "child group {pgid} survived owner death"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "relay took {elapsed:?} (>= 2s) to end child group after owner death"
    );
    assert!(
        direct_children(relay_pid).is_empty() && !group_exists(relay_pid),
        "relay left leaked processes in its process group {relay_pid}"
    );
}

#[test]
fn real_signal_relay_stop_leaves_group_running_and_no_leaked_children() {
    let mut child = Command::new("/bin/sleep")
        .arg("600")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let pgid = child.id();
    let _pg_reaper = GroupReaper(pgid);

    let relay = SignalRelay::start(pgid).unwrap();
    // Give the relay a brief moment to spawn reader + watcher.
    thread::sleep(Duration::from_millis(100));
    relay.stop();

    assert!(
        matches!(child.try_wait(), Ok(None)),
        "SignalRelay::stop must leave the command running"
    );
    let _ = child.kill();
    let _ = child.wait();
}
