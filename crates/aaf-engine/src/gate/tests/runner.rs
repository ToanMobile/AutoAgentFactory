use std::fs;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use aaf_domain::config::CommandsConfig;
use aaf_domain::gate::GateMode;
use tempfile::TempDir;

use super::super::proc::git_bytes;
use super::super::runner::{LOG_TAIL_BYTES, plan, run_all, run_command};

const LONG: Duration = Duration::from_secs(60);

fn alive(pid: &str) -> bool {
    Command::new("/bin/kill")
        .args(["-0", pid])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

#[test]
fn gate_git_never_starts_auto_gc() {
    let dir = TempDir::new().unwrap();
    let init = Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["init", "-q"])
        .env_remove("GIT_DIR")
        .output()
        .unwrap();
    assert!(init.status.success());
    // `git config --get` sees the `-c` values of its own invocation.
    let seen = git_bytes(dir.path(), &["config", "--get", "gc.auto"], LONG);
    assert_eq!(seen.ok().as_deref(), Some(b"0\n".as_slice()));
    // The flag must not swallow the caller's own `-c` options.
    let args = ["-c", "user.name=probe", "config", "--get", "user.name"];
    assert_eq!(git_bytes(dir.path(), &args, LONG).unwrap(), b"probe\n");
}

#[test]
fn plan_follows_the_mode() {
    let commands = CommandsConfig {
        build: "b".to_string(),
        test_scoped: "ts".to_string(),
        test_full: "tf".to_string(),
        lint: "l".to_string(),
        lint_full: "lf".to_string(),
        repro: "r".to_string(),
        autofix: String::new(),
        light_commands: Vec::new(),
    };
    let names = |mode| -> Vec<&str> { plan(&commands, mode).into_iter().map(|s| s.0).collect() };
    assert_eq!(names(GateMode::Scoped), vec!["lint", "test_scoped"]);
    assert_eq!(
        names(GateMode::Full),
        vec!["lint", "lint_full", "build", "test_full"]
    );
    assert_eq!(
        names(GateMode::Release),
        vec!["lint", "lint_full", "build", "test_full"]
    );
    let only_test = CommandsConfig {
        test_scoped: " ts ".to_string(),
        ..CommandsConfig::default()
    };
    assert_eq!(
        plan(&only_test, GateMode::Scoped),
        vec![("test_scoped", "ts".to_string())]
    );
    assert!(plan(&only_test, GateMode::Full).is_empty());
}

#[test]
fn command_runs_in_the_repo_root_and_captures_both_streams() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("marker.txt"), "x").unwrap();
    let result = run_command(dir.path(), "lint", "ls; echo oops >&2; exit 3", LONG).unwrap();
    assert_eq!(result.exit_code, Some(3));
    assert!(!result.timed_out);
    assert!(!result.passed());
    assert!(result.log_tail.contains("marker.txt"));
    assert!(result.log_tail.contains("oops"));
    assert_eq!(result.name, "lint");

    let ok = run_command(dir.path(), "lint", "true", LONG).unwrap();
    assert!(ok.passed());
}

#[test]
fn chatty_command_does_not_deadlock_and_keeps_only_the_tail() {
    let dir = TempDir::new().unwrap();
    // Far more than a pipe buffer on both streams.
    let script = "i=0; while [ $i -lt 20000 ]; do echo \"out line $i\"; \
                  echo \"err line $i\" >&2; i=$((i+1)); done; echo THE-END";
    let result = run_command(dir.path(), "test_full", script, LONG).unwrap();
    assert!(result.passed(), "{:?}", result.exit_code);
    assert!(result.log_tail.len() <= LOG_TAIL_BYTES);
    assert!(result.log_tail.len() > LOG_TAIL_BYTES / 2);
    assert!(result.log_tail.contains("THE-END"));
    assert!(!result.log_tail.contains("line 1\n"));
}

/// The two streams are one log: the tail must hold the lines in the order
/// the command wrote them, or the last line of one stream (the summary of a
/// test run) can be pushed out of the tail by late lines of the other.
#[test]
fn stdout_and_stderr_stay_in_the_order_they_were_written() {
    let dir = TempDir::new().unwrap();
    let script = "i=0; while [ $i -lt 3000 ]; do echo \"out $i\"; \
                  echo \"err $i\" >&2; i=$((i+1)); done; echo THE-END";
    let result = run_command(dir.path(), "test_full", script, LONG).unwrap();
    assert!(result.passed(), "{:?}", result.exit_code);
    // The first line of the tail may be cut; every later one is whole.
    let lines: Vec<&str> = result.log_tail.lines().skip(1).collect();
    assert!(lines.len() > 100, "{}", lines.len());
    assert_eq!(lines.last(), Some(&"THE-END"));
    for pair in lines.windows(2) {
        let follows = match pair[0].split_once(' ') {
            Some(("out", i)) => format!("err {i}"),
            Some(("err", "2999")) => "THE-END".to_string(),
            Some(("err", i)) => format!("out {}", i.parse::<u32>().unwrap() + 1),
            other => panic!("unexpected line {other:?}"),
        };
        assert_eq!(pair[1], follows, "after {:?}", pair[0]);
    }
}

#[test]
fn timeout_kills_the_whole_process_group() {
    let dir = TempDir::new().unwrap();
    let pid_file = dir.path().join("pid");
    // The sleeper is a grandchild: killing only `sh` would leave it running.
    let script = format!("sleep 300 & echo $! > '{}'; wait", pid_file.display());
    let started = Instant::now();
    let timeout = Duration::from_millis(400);
    let result = run_command(dir.path(), "test_scoped", &script, timeout).unwrap();
    assert!(result.timed_out);
    assert_eq!(result.exit_code, None);
    assert!(!result.passed());
    assert!(started.elapsed() < Duration::from_secs(30));

    let pid = fs::read_to_string(&pid_file).unwrap().trim().to_string();
    assert!(!pid.is_empty());
    // The kernel delivers SIGKILL asynchronously: poll with a deadline.
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(&pid) && Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert!(!alive(&pid), "sleep {pid} survived the timeout");
}

/// Ends a helper by the pid it wrote, whatever the test's outcome.
struct KillOnDrop(std::path::PathBuf);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Ok(pid) = fs::read_to_string(&self.0) {
            let _ = Command::new("/bin/kill")
                .args(["-KILL", pid.trim()])
                .status();
        }
    }
}

/// Shell text that leaves a process in its own process group holding
/// stdout and stderr open after the shell ends (what a copy of the pipe
/// leaked into another child does on macOS, see `aaf_fp::output`), then
/// writes `out`. Run with `/bin/bash` so `set -m` takes effect without a
/// controlling terminal (`dash` silently ignores `set -m`).
fn leaves_a_holder(pid_file: &std::path::Path) -> String {
    format!(
        "set -m; /bin/sleep 30 & echo $! > '{}'; echo out",
        pid_file.display()
    )
}

#[test]
fn another_holder_of_the_output_does_not_make_a_finished_capture_incomplete() {
    let dir = TempDir::new().unwrap();
    for with_stderr in [true, false] {
        let pid_file = dir.path().join(format!("holder-{with_stderr}.pid"));
        let _holder = KillOnDrop(pid_file.clone());
        let mut cmd = Command::new("/bin/bash");
        cmd.args(["-c", &leaves_a_holder(&pid_file)]);
        let done = super::super::proc::capture(cmd, LONG, None, with_stderr).unwrap();
        assert_eq!(done.exit_code, Some(0));
        assert!(done.complete, "with_stderr={with_stderr}");
        assert_eq!(done.output, b"out\n");
        let pid = fs::read_to_string(&pid_file).unwrap().trim().to_string();
        assert!(!pid.is_empty());
        assert!(
            alive(&pid),
            "holder {pid} in its own process group was killed (with_stderr={with_stderr})"
        );
    }
}

#[test]
fn a_finished_capture_kills_background_jobs_left_in_its_group() {
    let dir = TempDir::new().unwrap();
    let pid_file = dir.path().join("bg-capture.pid");
    let _holder = KillOnDrop(pid_file.clone());
    let mut cmd = Command::new("/bin/sh");
    cmd.args([
        "-c",
        &format!(
            "/bin/sleep 30 </dev/null >/dev/null 2>&1 & echo $! > '{}'; exit 0",
            pid_file.display()
        ),
    ]);
    let done = super::super::proc::capture(cmd, LONG, None, true).unwrap();
    assert_eq!(done.exit_code, Some(0));
    assert!(!done.timed_out);
    assert!(done.complete);

    let pid = fs::read_to_string(&pid_file).unwrap().trim().to_string();
    assert!(!pid.is_empty());
    let deadline = Instant::now() + Duration::from_secs(2);
    while alive(&pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !alive(&pid),
        "background sleep {pid} survived normal capture exit"
    );
}

#[test]
fn a_finished_run_supervised_kills_background_jobs_left_in_its_group() {
    struct Noop;
    impl super::super::proc::Supervise for Noop {
        fn tick(&mut self, _pgid: u32, _elapsed: Duration) -> bool {
            false
        }
    }

    let dir = TempDir::new().unwrap();
    let pid_file = dir.path().join("bg-run-supervised.pid");
    let _holder = KillOnDrop(pid_file.clone());
    let mut cmd = Command::new("/bin/sh");
    cmd.args([
        "-c",
        &format!(
            "/bin/sleep 30 </dev/null >/dev/null 2>&1 & echo $! > '{}'; exit 0",
            pid_file.display()
        ),
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    let (waited, unkilled) = super::super::proc::run_supervised(cmd, LONG, &mut Noop).unwrap();
    assert_eq!(waited.status.code(), Some(0));
    assert!(!waited.timed_out);
    assert_eq!(unkilled, None);

    let pid = fs::read_to_string(&pid_file).unwrap().trim().to_string();
    assert!(!pid.is_empty());
    let deadline = Instant::now() + Duration::from_secs(2);
    while alive(&pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !alive(&pid),
        "background sleep {pid} survived normal run_supervised exit"
    );
}

/// The error names the git command, not the `-c` option before it.
#[test]
fn a_failed_git_call_is_named_by_its_command() {
    let dir = TempDir::new().unwrap();
    let args = ["-c", "core.quotePath=false", "no-such-command"];
    let error = git_bytes(dir.path(), &args, LONG).unwrap_err().to_string();
    assert!(error.contains("no-such-command: exit"), "{error}");
}

#[test]
fn run_all_stops_at_the_first_failure() {
    let dir = TempDir::new().unwrap();
    let commands = CommandsConfig {
        lint: "true".to_string(),
        build: "exit 7".to_string(),
        test_full: "touch ran-tests".to_string(),
        ..CommandsConfig::default()
    };
    let results = run_all(dir.path(), &commands, GateMode::Full, LONG).unwrap();
    assert_eq!(results.len(), 2);
    assert!(results[0].passed());
    assert_eq!(results[1].exit_code, Some(7));
    assert!(!dir.path().join("ran-tests").exists());
}
