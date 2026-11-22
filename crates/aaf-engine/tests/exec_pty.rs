#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Real-process tests of the pty runner.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aaf_engine::executors::pty::{CommandSpec, PtyOutput, run_pty};
use aaf_engine::executors::reaper;

fn sh(script: &str) -> CommandSpec {
    CommandSpec {
        program: "/bin/sh".to_string(),
        args: vec!["-c".to_string(), script.to_string()],
        ..CommandSpec::default()
    }
}

fn run(script: &str, timeout_secs: u64) -> PtyOutput {
    run_pty(
        &sh(script),
        Duration::from_secs(timeout_secs),
        64 * 1024,
        None,
    )
    .unwrap()
}

/// Pid printed by the script as `child=<pid>`.
fn child_pid(tail: &str) -> u32 {
    tail.lines()
        .find_map(|line| line.trim().strip_prefix("child="))
        .expect("script printed child=<pid>")
        .parse()
        .unwrap()
}

/// Polls the process table (each poll is a `ps` round trip) until `pid` is gone.
fn assert_gone(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while reaper::is_alive(pid).unwrap() {
        assert!(Instant::now() < deadline, "pid {pid} is still alive");
        std::thread::yield_now();
    }
}

#[test]
fn echo_is_captured_with_exit_zero() {
    let out = run("echo hello-pty", 30);
    assert_eq!(out.exit_code, Some(0));
    assert!(!out.timed_out);
    assert!(out.tail.contains("hello-pty"), "{:?}", out.tail);
    assert!(out.pid.is_some());
}

#[test]
fn child_sees_a_terminal_cwd_and_env() {
    let dir = tempfile::tempdir().unwrap();
    let mut spec = sh("test -t 1 && echo tty=yes; echo var=$AAF_PTY_TEST; pwd");
    spec.cwd = Some(dir.path().to_string_lossy().into_owned());
    spec.env = vec![("AAF_PTY_TEST".to_string(), "v 1".to_string())];
    let out = run_pty(&spec, Duration::from_secs(30), 4096, None).unwrap();
    assert!(out.tail.contains("tty=yes"), "{:?}", out.tail);
    assert!(out.tail.contains("var=v 1"), "{:?}", out.tail);
    let real = dir.path().canonicalize().unwrap();
    assert!(out.tail.contains(real.to_str().unwrap()), "{:?}", out.tail);
}

#[test]
fn exit_code_is_propagated() {
    assert_eq!(run("exit 7", 30).exit_code, Some(7));
    assert_eq!(run("true", 30).exit_code, Some(0));
}

#[test]
fn death_by_signal_has_no_exit_code() {
    let out = run("kill -KILL $$", 30);
    assert_eq!(out.exit_code, None);
    assert!(!out.timed_out);
}

#[test]
fn missing_program_is_an_error() {
    let spec = CommandSpec {
        program: "/nonexistent/aaf-no-such-binary".to_string(),
        ..CommandSpec::default()
    };
    assert!(run_pty(&spec, Duration::from_secs(5), 1024, None).is_err());
}

#[test]
fn large_output_does_not_deadlock_and_tail_is_bounded() {
    // 5 MB is far more than any pty buffer: without the drain thread the
    // child would block on write and hit the timeout.
    let spec = sh("head -c 5000000 /dev/zero | tr '\\0' x; echo; echo END-MARK");
    let out = run_pty(&spec, Duration::from_secs(60), 1024, None).unwrap();
    assert!(!out.timed_out);
    assert_eq!(out.exit_code, Some(0));
    assert!(out.tail.len() <= 1024, "tail is {} bytes", out.tail.len());
    assert!(out.tail.trim_end().ends_with("END-MARK"), "{:?}", out.tail);
}

#[test]
fn line_callback_gets_each_line_without_terminators() {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&lines);
    let out = run_pty(
        &sh("printf 'one\\ntwo words\\n\\nlast-no-newline'"),
        Duration::from_secs(30),
        4096,
        Some(Box::new(move |line| {
            sink.lock().unwrap().push(line.to_string())
        })),
    )
    .unwrap();
    assert_eq!(out.exit_code, Some(0));
    assert_eq!(
        *lines.lock().unwrap(),
        vec!["one", "two words", "", "last-no-newline"]
    );
}

#[test]
fn timeout_kills_child_and_grandchild() {
    let started = Instant::now();
    let out = run("sleep 300 & echo child=$!; sleep 300", 1);
    assert!(out.timed_out);
    assert_eq!(out.exit_code, None);
    assert!(started.elapsed() < Duration::from_secs(60));
    assert_gone(child_pid(&out.tail));
    assert_gone(out.pid.unwrap());
}

#[test]
fn timeout_kills_child_that_ignores_sigterm() {
    let out = run(
        "trap '' TERM; echo child=$$; while :; do sleep 300; done",
        1,
    );
    assert!(out.timed_out);
    assert_gone(child_pid(&out.tail));
}

#[test]
fn timeout_kills_grandchild_in_its_own_session() {
    // The grandchild leaves the process group with setsid, so only the
    // descendant tracker can find it.
    let script = "/usr/bin/perl -MPOSIX -e '$p = fork; if (!$p) { setsid; exec \"/bin/sleep\", \"300\" } $| = 1; print \"child=$p\\n\"; sleep 300'";
    let out = run(script, 1);
    assert!(out.timed_out);
    assert_gone(child_pid(&out.tail));
}

#[test]
fn background_job_left_by_a_finished_child_is_killed() {
    let out = run("sleep 300 & echo child=$!", 60);
    assert!(!out.timed_out);
    assert_eq!(out.exit_code, Some(0));
    assert_gone(child_pid(&out.tail));
}
