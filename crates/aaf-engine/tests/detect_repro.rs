#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use aaf_engine::detector::repro::{DEFAULT_RUNS, ReproCommand, ReproOutcome, verify};
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(30);

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    path
}

/// Runs the script through `/bin/sh <file>`: argv form, no `-c`.
fn sh(dir: &TempDir, body: &str, timeout: Duration) -> ReproCommand {
    let path = script(dir.path(), "repro.sh", body);
    ReproCommand::argv(
        "/bin/sh",
        vec![path.to_string_lossy().into_owned()],
        dir.path(),
        timeout,
    )
}

fn run_count(dir: &TempDir) -> usize {
    fs::read_to_string(dir.path().join("runs"))
        .unwrap_or_default()
        .lines()
        .count()
}

#[test]
fn default_is_two_runs() {
    assert_eq!(DEFAULT_RUNS, 2);
}

#[test]
fn confirmed_when_every_run_fails() {
    let dir = TempDir::new().unwrap();
    let cmd = sh(&dir, "echo run >> runs\nexit 3", TIMEOUT);
    assert_eq!(verify(&cmd, DEFAULT_RUNS), ReproOutcome::Confirmed);
    assert_eq!(run_count(&dir), 2, "runs in the given cwd, twice");
}

#[test]
fn not_reproduced_when_every_run_passes() {
    let dir = TempDir::new().unwrap();
    let cmd = sh(&dir, "echo run >> runs\nexit 0", TIMEOUT);
    assert_eq!(verify(&cmd, DEFAULT_RUNS), ReproOutcome::NotReproduced);
    assert_eq!(run_count(&dir), 2);
}

#[test]
fn flaky_when_first_run_fails_and_second_passes() {
    let dir = TempDir::new().unwrap();
    let cmd = sh(
        &dir,
        "echo run >> runs\n[ \"$(wc -l < runs)\" -ge 2 ]",
        TIMEOUT,
    );
    assert_eq!(verify(&cmd, DEFAULT_RUNS), ReproOutcome::Flaky);
}

#[test]
fn flaky_when_first_run_passes_and_second_fails() {
    let dir = TempDir::new().unwrap();
    let cmd = sh(
        &dir,
        "echo run >> runs\n[ \"$(wc -l < runs)\" -lt 2 ]",
        TIMEOUT,
    );
    assert_eq!(verify(&cmd, DEFAULT_RUNS), ReproOutcome::Flaky);
}

#[test]
fn one_pass_in_many_runs_is_flaky_not_confirmed() {
    let dir = TempDir::new().unwrap();
    let cmd = sh(
        &dir,
        "echo run >> runs\n[ \"$(wc -l < runs)\" -eq 4 ]",
        TIMEOUT,
    );
    assert_eq!(verify(&cmd, 5), ReproOutcome::Flaky);
    assert_eq!(run_count(&dir), 5);
}

#[test]
fn death_by_signal_counts_as_a_failed_run() {
    let dir = TempDir::new().unwrap();
    let cmd = sh(&dir, "kill -ABRT $$", TIMEOUT);
    assert_eq!(verify(&cmd, DEFAULT_RUNS), ReproOutcome::Confirmed);
}

fn alive(pid: &str) -> bool {
    Command::new("/bin/kill")
        .args(["-0", pid])
        .output()
        .unwrap()
        .status
        .success()
}

#[test]
fn timeout_kills_the_whole_process_group() {
    let dir = TempDir::new().unwrap();
    // The background sleep is a grandchild of the test: only a group kill
    // reaches it.
    let cmd = sh(
        &dir,
        "echo run >> runs\nsleep 600 &\necho $! > child.pid\nwait",
        Duration::from_millis(1500),
    );
    let started = Instant::now();
    assert_eq!(verify(&cmd, DEFAULT_RUNS), ReproOutcome::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(60));
    assert_eq!(run_count(&dir), 1, "no second run after a timeout");

    let pid = fs::read_to_string(dir.path().join("child.pid")).unwrap();
    let pid = pid.trim();
    assert!(!pid.is_empty());
    // SIGKILL delivery is asynchronous; bounded wait for the kernel.
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(pid) && Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert!(!alive(pid), "grandchild {pid} survived the timeout");
}

#[test]
fn missing_program_is_unrunnable() {
    let dir = TempDir::new().unwrap();
    let cmd = ReproCommand::argv(
        dir.path().join("no-such-program").to_string_lossy(),
        Vec::new(),
        dir.path(),
        TIMEOUT,
    );
    assert!(matches!(
        verify(&cmd, DEFAULT_RUNS),
        ReproOutcome::Unrunnable { .. }
    ));
}

#[test]
fn zero_runs_confirm_nothing() {
    let dir = TempDir::new().unwrap();
    let cmd = sh(&dir, "exit 1", TIMEOUT);
    assert!(matches!(verify(&cmd, 0), ReproOutcome::Unrunnable { .. }));
}

#[test]
fn argv_arguments_are_never_interpreted_by_a_shell() {
    let dir = TempDir::new().unwrap();
    let path = script(dir.path(), "repro.sh", "printf '%s' \"$1\" > arg\nexit 1");
    let hostile = "$(touch pwned); touch pwned2 `touch pwned3`";
    let cmd = ReproCommand::argv(
        "/bin/sh",
        vec![path.to_string_lossy().into_owned(), hostile.to_string()],
        dir.path(),
        TIMEOUT,
    );
    assert!(!cmd.uses_shell());
    assert_eq!(verify(&cmd, 1), ReproOutcome::Confirmed);
    assert_eq!(fs::read_to_string(dir.path().join("arg")).unwrap(), hostile);
    for name in ["pwned", "pwned2", "pwned3"] {
        assert!(!dir.path().join(name).exists(), "{name} was created");
    }
}

#[test]
fn committed_shell_string_runs_through_sh() {
    let dir = TempDir::new().unwrap();
    let cmd = ReproCommand::committed_shell("touch ran && test -f missing", dir.path(), TIMEOUT);
    assert!(cmd.uses_shell());
    assert_eq!(verify(&cmd, DEFAULT_RUNS), ReproOutcome::Confirmed);
    assert!(dir.path().join("ran").exists());

    let pass = ReproCommand::committed_shell("test -f ran && true", dir.path(), TIMEOUT);
    assert_eq!(verify(&pass, DEFAULT_RUNS), ReproOutcome::NotReproduced);
}
