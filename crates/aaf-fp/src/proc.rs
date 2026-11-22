//! One external command with a deadline.

use std::io::{self, Read};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::FpError;
use crate::output::{self, Exited};

/// How long the output may still take once the command has exited. Reading
/// ends as soon as nothing the command wrote is left ([`crate::output`]),
/// however long other copies of the stream stay open; the bound covers a
/// reader thread starved on a loaded machine, and a holder that goes on
/// writing.
const DRAIN_GRACE: Duration = Duration::from_secs(30);
/// How much of the command's stderr an error message carries.
const STDERR_KEPT: usize = 400;

pub(crate) enum Failure {
    /// The command ran and said no.
    Exit {
        code: Option<i32>,
        stderr: String,
    },
    Other(FpError),
}

impl Failure {
    pub(crate) fn into_error(self, label: &str) -> FpError {
        match self {
            Failure::Exit { code, stderr } if stderr.is_empty() => {
                FpError::Git(format!("{label}: exit {code:?}"))
            }
            Failure::Exit { code, stderr } => {
                FpError::Git(format!("{label}: exit {code:?}: {stderr}"))
            }
            Failure::Other(error) => error,
        }
    }
}

impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Failure {
        Failure::Other(error.into())
    }
}

/// Reads `pipe` to its end on a thread of its own: output larger than the
/// pipe buffer would otherwise block the child until the deadline.
fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> mpsc::Receiver<io::Result<Vec<u8>>> {
    let (tx, rx) = mpsc::channel();
    if let Some(mut pipe) = pipe {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let read = pipe.read_to_end(&mut bytes).map(|_| bytes);
            // The receiver is gone only when nobody wants the data any more.
            let _ = tx.send(read);
        });
    }
    rx
}

/// The start of what the command wrote to stderr, on one line: git puts the
/// error first and a page of usage or hints after it.
fn stderr_head(rx: &mpsc::Receiver<io::Result<Vec<u8>>>) -> String {
    // A holder that goes on writing must not hang the caller: after the
    // grace the message goes without the text.
    let Ok(Ok(bytes)) = rx.recv_timeout(DRAIN_GRACE) else {
        return String::new();
    };
    let text = String::from_utf8_lossy(&bytes);
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    line.chars().take(STDERR_KEPT).collect()
}

/// Runs `cmd` to its end and returns its stdout as written. `input`, when
/// given, is what the command reads from stdin.
pub(crate) fn run(
    mut cmd: Command,
    label: &str,
    input: Option<Vec<u8>>,
    timeout: Duration,
) -> Result<String, Failure> {
    // Set once the child is reaped, or on any return before that.
    let exited = Exited::new();
    let (stdout_end, stdout) = output::output(&exited)?;
    let (stderr_end, stderr) = output::output(&exited)?;
    let input = match input {
        Some(bytes) => {
            let (stdin_end, writer) = output::input()?;
            cmd.stdin(stdin_end);
            Some((writer, bytes))
        }
        None => {
            cmd.stdin(Stdio::null());
            None
        }
    };
    cmd.stdout(stdout_end).stderr(stderr_end);
    let mut child = cmd.spawn()?;
    // The command holds this process's copies of the child's ends.
    drop(cmd);
    if let Some((writer, bytes)) = input {
        std::thread::spawn(move || {
            // A command that exits without reading everything closes the
            // stream; its exit status is what reports that.
            let _ = writer.finish(&bytes);
        });
    }
    let stdout = drain(Some(stdout));
    let stderr = drain(Some(stderr));
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            // Best effort: the child may already be gone.
            let _ = child.kill();
            let _ = child.wait();
            return Err(Failure::Other(FpError::Git(format!("{label}: timed out"))));
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    exited.mark();
    if !status.success() {
        return Err(Failure::Exit {
            code: status.code(),
            stderr: stderr_head(&stderr),
        });
    }
    let stdout = stdout.recv_timeout(DRAIN_GRACE).map_err(|_| {
        Failure::Other(FpError::Git(format!(
            "{label}: output still arriving after the command exited"
        )))
    })??;
    String::from_utf8(stdout)
        .map_err(|_| Failure::Other(FpError::Git(format!("{label}: output is not UTF-8"))))
}
