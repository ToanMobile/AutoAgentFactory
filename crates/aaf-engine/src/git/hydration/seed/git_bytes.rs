//! Timeout-bounded raw-byte git command runner for seed checks (S2.4 / S2.8).
//!
//! Preserves exact stdout bytes without lossy UTF-8 conversion and kills the
//! child process group on timeout without using `unsafe`.

use std::io::{self, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use aaf_fp::output::{self, Exited};

const PIPE_GRACE: Duration = Duration::from_secs(5);
const FAST_POLL_FOR: Duration = Duration::from_millis(50);
const FAST_POLL: Duration = Duration::from_millis(1);
const FIRST_BACKOFF: Duration = Duration::from_millis(2);
const MAX_BACKOFF: Duration = Duration::from_millis(10);

const SCRUBBED_ENV: [&str; 6] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_PREFIX",
];

#[derive(Debug, Clone)]
pub(super) struct RawGitOutput {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl RawGitOutput {
    pub(super) fn success(&self) -> bool {
        self.code == Some(0)
    }
}

fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    if let Some(mut pipe) = pipe {
        thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            let _ = tx.send(buf);
        });
    }
    rx
}

fn kill_process_group(child: &mut Child) {
    #[cfg(unix)]
    {
        let group = format!("-{}", child.id());
        let _ = Command::new("/bin/kill")
            .args(["-KILL", "--", &group])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
}

pub(super) fn remaining_time(deadline: Instant) -> Result<Duration, String> {
    let rem = deadline
        .checked_duration_since(Instant::now())
        .unwrap_or(Duration::ZERO);
    if rem.is_zero() {
        Err("timeout".to_string())
    } else {
        Ok(rem)
    }
}

/// Testable wrapper exposing [`git_raw_bytes`] for real-git deadline verification.
#[doc(hidden)]
pub fn run_git_raw_bytes_timed(
    dir: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    let out = git_raw_bytes(dir, args, Instant::now() + timeout)?;
    if out.success() {
        Ok(out.stdout)
    } else {
        Err(format!("git exited {:?}: {}", out.code, out.stderr.trim()))
    }
}

/// Runs `git -C <dir> <args>` bounded by `deadline` and returns exact raw stdout bytes.
pub(super) fn git_raw_bytes(
    dir: &Path,
    args: &[&str],
    deadline: Instant,
) -> Result<RawGitOutput, String> {
    let timeout = remaining_time(deadline)?;
    let git_bin = aaf_fp::program("git").map_err(|e| format!("git_program: {e}"))?;
    let mut cmd = Command::new(git_bin);
    cmd.arg("-C")
        .arg(dir)
        .args(["-c", "gc.auto=0", "-c", "core.fsmonitor=false"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0");
    for var in SCRUBBED_ENV {
        cmd.env_remove(var);
    }

    let exited = Exited::new();
    let (stdout_end, stdout) = output::output(&exited).map_err(|e| format!("git_pipe: {e}"))?;
    let (stderr_end, stderr) = output::output(&exited).map_err(|e| format!("git_pipe: {e}"))?;
    cmd.stdin(Stdio::null())
        .stdout(stdout_end)
        .stderr(stderr_end);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| format!("git_spawn: {e}"))?;
    cmd.stdout(Stdio::null()).stderr(Stdio::null());

    let stdout_rx = drain(Some(stdout));
    let stderr_rx = drain(Some(stderr));
    let started = Instant::now();
    let run_deadline = started + timeout;
    let mut backoff = FIRST_BACKOFF;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(err) => {
                kill_process_group(&mut child);
                let _ = child.wait();
                return Err(format!("git_wait: {err}"));
            }
        }
        let now = Instant::now();
        if now >= run_deadline {
            kill_process_group(&mut child);
            let _ = child.wait();
            return Err("timeout".to_string());
        }
        let poll = if now - started < FAST_POLL_FOR {
            FAST_POLL
        } else {
            let p = backoff;
            backoff = (backoff * 2).min(MAX_BACKOFF);
            p
        };
        thread::sleep(poll.min(run_deadline - now));
    };
    exited.mark();
    let grace = PIPE_GRACE.min(
        deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::from_millis(50)),
    );
    let stdout_bytes = stdout_rx
        .recv_timeout(grace)
        .map_err(|_| io::Error::other("git stdout did not finish").to_string())?;
    let stderr_bytes = stderr_rx.recv_timeout(grace).unwrap_or_default();
    Ok(RawGitOutput {
        code: status.code(),
        stdout: stdout_bytes,
        stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
    })
}
