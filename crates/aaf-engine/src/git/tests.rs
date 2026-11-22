use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use super::cmd;

#[test]
fn git_command_disables_gc_and_scrubs_location_env() {
    let command = cmd::git_command(Path::new("/some/repo"), &["status"]).unwrap();
    let args: Vec<&OsStr> = command.get_args().collect();
    assert_eq!(args, ["-C", "/some/repo", "-c", "gc.auto=0", "status"]);
    for var in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"] {
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == OsStr::new(var) && value.is_none()),
            "{var} is not removed"
        );
    }
}

/// The program is the absolute path looked up once per process, so a `git`
/// that appears in a `PATH` directory later is never the one run.
#[test]
fn git_command_runs_the_git_resolved_once_not_a_name_for_path_lookup() {
    let command = cmd::git_command(Path::new("/some/repo"), &["status"]).unwrap();
    let program = Path::new(command.get_program());
    assert!(program.is_absolute(), "{program:?}");
    assert_eq!(program, aaf_fp::program("git").unwrap());
}

#[test]
fn run_captures_stdout_stderr_and_exit_code() {
    let out = cmd::run(
        Command::new("/bin/sh").args(["-c", "echo out; echo err >&2; exit 3"]),
        Duration::from_secs(10),
    )
    .unwrap();
    assert_eq!(out.code, Some(3));
    assert_eq!(out.stdout, "out\n");
    assert_eq!(out.stderr, "err\n");
    assert!(!out.success());
}

/// A command that exited has all its output read, even while another
/// process still holds the streams (a copy of a pipe that leaked into a
/// child another thread spawned: `aaf_fp::output`). It used to come back
/// as a success with empty stdout.
#[test]
fn run_reads_all_output_while_another_holder_keeps_the_streams_open() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("holder.pid");
    let script = format!(
        "/bin/sleep 30 & echo $! > '{}'; echo out; echo err >&2",
        pid_file.display()
    );
    let out = cmd::run(
        Command::new("/bin/sh").args(["-c", &script]),
        Duration::from_secs(60),
    );
    if let Ok(pid) = std::fs::read_to_string(&pid_file) {
        let _ = Command::new("/bin/kill")
            .args(["-KILL", pid.trim()])
            .status();
    }
    let out = out.unwrap();
    assert_eq!(out.code, Some(0));
    assert_eq!(out.stdout, "out\n");
    assert_eq!(out.stderr, "err\n");
}

#[test]
fn run_kills_a_command_that_exceeds_its_timeout() {
    let err = cmd::run(
        Command::new("/bin/sleep").arg("30"),
        Duration::from_millis(50),
    )
    .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
}

#[test]
fn failed_git_call_reports_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let err = cmd::git(dir.path(), &["rev-parse", "HEAD"]).unwrap_err();
    match err {
        super::GitError::Command {
            command, stderr, ..
        } => {
            assert_eq!(command, "git rev-parse HEAD");
            assert!(stderr.contains("not a git repository"), "{stderr}");
        }
        other => panic!("unexpected error: {other}"),
    }
}
