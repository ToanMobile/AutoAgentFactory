//! The single place that spawns git (and any other external command of this
//! layer): bounded by a timeout, with stderr captured for error messages.

use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use aaf_fp::output::{self, Exited};

use super::GitError;

/// Budget for one git call, network calls included.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(120);

/// How long the output may still take once the process has exited. The
/// readers end with what the process wrote, however long a background
/// helper (or a copy of the stream leaked into another child) keeps the
/// stream open; only a holder that goes on writing, or a reader starved
/// this long, exceeds it.
const PIPE_GRACE: Duration = Duration::from_secs(30);

/// Longest stderr kept in an error message.
const STDERR_TAIL: usize = 2000;

/// Location variables a git hook exports. Left in place they would point
/// our commands at the hook's repository instead of `-C <dir>`.
const SCRUBBED_ENV: [&str; 6] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_PREFIX",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// `None` when the process was killed by a signal.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    if let Some(mut pipe) = pipe {
        thread::spawn(move || {
            let mut buf = Vec::new();
            // A read error only ends the capture early; what was read so far
            // is still the best error text available.
            let _ = pipe.read_to_end(&mut buf);
            // The receiver is gone only when the caller already gave up.
            let _ = tx.send(buf);
        });
    }
    rx
}

/// Runs `cmd` to completion with stdin closed. A run that exceeds `timeout`
/// is killed and reported as `io::ErrorKind::TimedOut`.
pub fn run(cmd: &mut Command, timeout: Duration) -> io::Result<Output> {
    // Set once the child is reaped (or on any return before): the readers
    // then end with what the child wrote, even while a copy of a stream
    // that leaked into another child keeps it open ([`aaf_fp::output`]).
    let exited = Exited::new();
    let (stdout_end, stdout) = output::output(&exited)?;
    let (stderr_end, stderr) = output::output(&exited)?;
    cmd.stdin(Stdio::null())
        .stdout(stdout_end)
        .stderr(stderr_end);
    let mut child = cmd.spawn()?;
    // The builder is the caller's and would keep this process's copies of
    // the child's ends open for as long as it lives.
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    // Both streams are drained on threads so a chatty child cannot block on
    // a full buffer while we wait for it.
    let stdout = drain(Some(stdout));
    let stderr = drain(Some(stderr));
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            // Best effort: the child may have exited in between.
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(io::ErrorKind::TimedOut, "timed out"));
        }
        thread::sleep(Duration::from_millis(2));
    };
    exited.mark();
    // Empty stdout would read as an answer ("nothing changed"): output that
    // did not end is an error. Stderr is only ever error text.
    let stdout = stdout
        .recv_timeout(PIPE_GRACE)
        .map_err(|_| io::Error::other("output still arriving after the command exited"))?;
    let text = |bytes: Vec<u8>| String::from_utf8_lossy(&bytes).into_owned();
    Ok(Output {
        code: status.code(),
        stdout: text(stdout),
        stderr: text(stderr.recv_timeout(PIPE_GRACE).unwrap_or_default()),
    })
}

/// The git invocation shared by every call of this layer. The program is
/// the absolute path looked up once for this process ([`aaf_fp::program`]),
/// never a name for the spawn to look up on `PATH` again.
pub(crate) fn git_command(dir: &Path, args: &[&str]) -> io::Result<Command> {
    let mut cmd = Command::new(aaf_fp::program("git")?);
    // Many worktrees share one object store; a gc started by one command
    // must not run under another task's feet.
    cmd.arg("-C").arg(dir).args(["-c", "gc.auto=0"]).args(args);
    for var in SCRUBBED_ENV {
        cmd.env_remove(var);
    }
    // Nobody is there to answer a credential prompt; fail instead of hanging.
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    Ok(cmd)
}

/// Builds the error for a git call that exited non-zero.
pub(crate) fn failure(args: &[&str], out: &Output) -> GitError {
    let stderr = out.stderr.trim();
    let start = stderr
        .char_indices()
        .rev()
        .nth(STDERR_TAIL - 1)
        .map_or(0, |(i, _)| i);
    GitError::Command {
        command: format!("git {}", args.join(" ")),
        code: out.code,
        stderr: stderr[start..].to_string(),
    }
}

/// Runs git in `dir` and returns its output whatever the exit code.
pub fn git_raw(dir: &Path, args: &[&str]) -> Result<Output, GitError> {
    git_command(dir, args)
        .and_then(|mut cmd| run(&mut cmd, GIT_TIMEOUT))
        .map_err(|err| {
            if err.kind() == io::ErrorKind::TimedOut {
                GitError::Timeout {
                    command: format!("git {}", args.join(" ")),
                }
            } else {
                GitError::Io(err)
            }
        })
}

/// Runs git in `dir`; a non-zero exit is an error carrying stderr. Returns
/// trimmed stdout.
pub fn git(dir: &Path, args: &[&str]) -> Result<String, GitError> {
    let out = git_raw(dir, args)?;
    if out.success() {
        Ok(out.stdout.trim().to_string())
    } else {
        Err(failure(args, &out))
    }
}
