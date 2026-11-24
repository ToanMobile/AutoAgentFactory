#![allow(clippy::unwrap_used, clippy::expect_used)]
//! R10-L8-1: exit 126/127 means the repro command never ran (missing program
//! under a wrapper, typo'd script, non-executable file) — not that the bug
//! reproduced.

use std::fs;
use std::time::Duration;

use aaf_engine::detector::repro::{DEFAULT_RUNS, ReproCommand, ReproOutcome, verify};
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(30);

fn sh_c(dir: &TempDir, script: &str) -> ReproCommand {
    ReproCommand::argv(
        "/bin/sh",
        vec!["-c".to_string(), script.to_string()],
        dir.path(),
        TIMEOUT,
    )
}

#[test]
fn exit_127_is_unrunnable_not_confirmed() {
    let dir = TempDir::new().unwrap();
    let outcome = verify(&sh_c(&dir, "exit 127"), DEFAULT_RUNS);
    match outcome {
        ReproOutcome::Unrunnable { reason } => {
            assert!(reason.contains("exit 127"), "{reason}");
            assert!(reason.contains("not found"), "{reason}");
        }
        other => panic!("exit 127 must be Unrunnable, got {other:?}"),
    }
}

#[test]
fn exit_126_is_unrunnable_not_confirmed() {
    let dir = TempDir::new().unwrap();
    let outcome = verify(&sh_c(&dir, "exit 126"), DEFAULT_RUNS);
    match outcome {
        ReproOutcome::Unrunnable { reason } => {
            assert!(reason.contains("exit 126"), "{reason}");
            assert!(reason.contains("not executable"), "{reason}");
        }
        other => panic!("exit 126 must be Unrunnable, got {other:?}"),
    }
}

#[test]
fn missing_script_under_sh_is_unrunnable() {
    let dir = TempDir::new().unwrap();
    let cmd = ReproCommand::argv(
        "/bin/sh",
        vec!["missing.sh".to_string()],
        dir.path(),
        TIMEOUT,
    );
    // bash exits 127 for a missing script but dash (Debian /bin/sh) exits
    // 2: the file is checked before running, so both shells agree.
    match verify(&cmd, DEFAULT_RUNS) {
        ReproOutcome::Unrunnable { reason } => {
            assert!(reason.contains("script not found: missing.sh"), "{reason}");
        }
        other => panic!("a missing script must be Unrunnable, got {other:?}"),
    }
    // An inline script (`sh -c`) is not a file and is never checked.
    let inline = ReproCommand::argv(
        "/bin/sh",
        vec!["-c".to_string(), "exit 1".to_string()],
        dir.path(),
        TIMEOUT,
    );
    assert_eq!(verify(&inline, DEFAULT_RUNS), ReproOutcome::Confirmed);
}

#[test]
fn a_real_failure_is_still_confirmed_and_one_unrunnable_run_wins_over_flaky() {
    let dir = TempDir::new().unwrap();
    assert_eq!(
        verify(&sh_c(&dir, "echo boom >&2; exit 1"), DEFAULT_RUNS),
        ReproOutcome::Confirmed
    );
    // First run fails for real, second run cannot find the program.
    let counter = dir.path().join("n");
    let script = format!(
        "if [ -f '{c}' ]; then exit 127; fi; : > '{c}'; exit 1",
        c = counter.display()
    );
    assert!(matches!(
        verify(&sh_c(&dir, &script), DEFAULT_RUNS),
        ReproOutcome::Unrunnable { .. }
    ));
    assert!(fs::metadata(&counter).is_ok());
}
