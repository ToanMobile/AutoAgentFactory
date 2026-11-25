//! Runs one external command with a deadline, in its own process group, so a
//! hung command and everything it spawned can be killed together. Also the
//! one [`CommandRunner`] seam through which the engine calls tools such as
//! `gh`, so tests can substitute them.

use std::ffi::OsStr;
use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use aaf_fp::output::{self, Exited};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// `None` when the process was killed by a signal.
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutput {
    pub fn success(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// Runs an external program with an argument vector (never a shell line) to
/// completion. The implementation owns the timeout. Injected so tests never
/// call the real tool.
pub trait CommandRunner {
    /// `io::ErrorKind::NotFound` means the program is not installed,
    /// `TimedOut` that it did not finish in time.
    fn run(&self, program: &str, args: &[&str], cwd: &Path) -> io::Result<CommandOutput>;
}

/// Output kept per stream. More than this is read and discarded so the child
/// never blocks on a full pipe.
const MAX_CAPTURE_BYTES: u64 = 8 * 1024 * 1024;

/// How long the readers may still take once the child has exited. They end
/// with what the child wrote whoever else holds the streams; only a holder
/// that goes on writing, or a reader thread starved this long, exceeds it.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

pub(crate) struct Finished {
    /// `None` when the deadline passed and the group was killed.
    pub status: Option<ExitStatus>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// SIGKILL to the whole group through `/bin/kill`: `std` can only signal the
/// direct child, and `libc::kill` would need `unsafe`.
fn kill_group(child: &mut Child) {
    let group = format!("-{}", child.id());
    // Both results are ignored on purpose: the usual failure is "no such
    // process" because the group ended by itself, and the caller's `wait`
    // reports the real outcome either way.
    let _ = Command::new("/bin/kill")
        .args(["-KILL", "--", &group])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.kill();
}

fn capture<R: Read + Send + 'static>(stream: Option<R>) -> Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    if let Some(stream) = stream {
        thread::spawn(move || {
            let mut kept = Vec::new();
            let mut limited = stream.take(MAX_CAPTURE_BYTES);
            // A read error ends the capture with what arrived so far; the
            // caller still gets the exit status and parses strictly.
            let _ = limited.read_to_end(&mut kept);
            let _ = io::copy(&mut limited.into_inner(), &mut io::sink());
            // The receiver is gone only when the caller gave up waiting.
            let _ = tx.send(kept);
        });
    }
    rx
}

/// Spawns `command` (stdin closed) and waits at most `timeout`.
pub(crate) fn run_with_timeout(
    mut command: Command,
    timeout: Duration,
    capture_output: bool,
) -> io::Result<Finished> {
    // Set once the child is reaped (or on any return before): the readers
    // then end with what the child wrote, even while a copy of a stream
    // that leaked into another child keeps it open ([`aaf_fp::output`]).
    let exited = Exited::new();
    let (stdout, stderr) = if capture_output {
        let (stdout_end, stdout) = output::output(&exited)?;
        let (stderr_end, stderr) = output::output(&exited)?;
        command.stdout(stdout_end).stderr(stderr_end);
        (Some(stdout), Some(stderr))
    } else {
        command.stdout(Stdio::null()).stderr(Stdio::null());
        (None, None)
    };
    command.stdin(Stdio::null()).process_group(0);
    let mut child = command.spawn()?;
    // The command holds this process's copies of the child's ends.
    drop(command);
    let stdout = capture(stdout);
    let stderr = capture(stderr);

    let deadline = Instant::now() + timeout;
    let mut poll = Duration::from_millis(1);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(err) => {
                kill_group(&mut child);
                return Err(err);
            }
        }
        let now = Instant::now();
        if now >= deadline {
            kill_group(&mut child);
            child.wait()?;
            break None;
        }
        thread::sleep(poll.min(deadline - now));
        poll = (poll * 2).min(Duration::from_millis(10));
    };
    exited.mark();
    let Some(status) = status else {
        return Ok(Finished {
            status: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
        });
    };
    if !capture_output {
        return Ok(Finished {
            status: Some(status),
            stdout: Vec::new(),
            stderr: Vec::new(),
        });
    }
    match (
        stdout.recv_timeout(DRAIN_GRACE),
        stderr.recv_timeout(DRAIN_GRACE),
    ) {
        (Ok(stdout), Ok(stderr)) => Ok(Finished {
            status: Some(status),
            stdout,
            stderr,
        }),
        _ => {
            // Output is incomplete, so the result cannot be trusted.
            kill_group(&mut child);
            Ok(Finished {
                status: None,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }
}

/// What [`SystemRunner`] spawns for `program`: `git` and `gh` are the
/// absolute paths looked up once for this process ([`aaf_fp::program`]), so
/// a file that appears in a `PATH` directory later is never run; `NotFound`
/// when there was none. Any other program is spawned as given.
pub fn spawn_program(program: &str) -> io::Result<&OsStr> {
    match program {
        "git" | "gh" => aaf_fp::program(program).map(Path::as_os_str),
        _ => Ok(OsStr::new(program)),
    }
}

/// [`CommandRunner`] that spawns the real program with a timeout.
#[derive(Debug, Clone)]
pub struct SystemRunner {
    pub timeout: Duration,
}

impl CommandRunner for SystemRunner {
    fn run(&self, program: &str, args: &[&str], cwd: &Path) -> io::Result<CommandOutput> {
        let mut command = Command::new(spawn_program(program)?);
        command.args(args).current_dir(cwd);
        let finished = run_with_timeout(command, self.timeout, true)
            .map_err(|err| io::Error::new(err.kind(), format!("cannot run {program}: {err}")))?;
        let Some(status) = finished.status else {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "{program} did not finish within {}s",
                    self.timeout.as_secs()
                ),
            ));
        };
        // Callers parse stdout strictly, so mangled bytes are refused here.
        let stdout = String::from_utf8(finished.stdout).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{program} wrote non-UTF-8 output"),
            )
        })?;
        Ok(CommandOutput {
            exit_code: status.code(),
            stdout,
            stderr: String::from_utf8_lossy(&finished.stderr).into_owned(),
        })
    }
}

/// Fail-closed check whether POSIX process `pid` is currently alive.
///
/// Returns `true` for `pid <= 1`, `pid == std::process::id()`, or any error /
/// timeout / unexpected status from `/bin/ps`. Returns `false` **only** when
/// `/bin/ps -p <pid> -o pid=` exits cleanly with status `1` and empty stdout.
pub fn process_alive(pid: u32) -> bool {
    if pid <= 1 || pid == std::process::id() {
        return true;
    }
    let pid_arg = pid.to_string();
    let mut cmd = Command::new("/bin/ps");
    cmd.args(["-p", &pid_arg, "-o", "pid="]);
    let Ok(finished) = run_with_timeout(cmd, Duration::from_secs(5), true) else {
        return true;
    };
    let exited_missing = finished.status.and_then(|s| s.code()) == Some(1);
    let empty_stdout = String::from_utf8_lossy(&finished.stdout).trim().is_empty();
    !(exited_missing && empty_stdout)
}
