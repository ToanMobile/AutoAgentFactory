//! Runs one command under a pseudo-terminal with a hard deadline.
//!
//! The child is a session and process-group leader (`portable-pty` calls
//! `setsid`), so its pid doubles as the process-group id that
//! [`reaper::kill_group`] signals. Output is drained on its own thread so a
//! chatty child can never fill the pty buffer and stall.

use std::collections::VecDeque;
use std::io::Read;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use aaf_domain::ports::{PortError, PortResult};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use super::reaper::{self, ProcessTracker, Signal};

/// Bytes of output kept for the run log.
pub const DEFAULT_TAIL_BYTES: usize = 64 * 1024;
/// Larger bounded output cap for generic CLIs whose answer lives inside raw stdout/stderr.
pub const GENERIC_TAIL_BYTES: usize = 256 * 1024;

/// How long a child gets to exit after SIGTERM before SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(2);
/// How long to wait for the pty to report end-of-file once the child is gone.
const DRAIN_GRACE: Duration = Duration::from_secs(2);
/// How often the descendant tree is sampled while the child runs.
const TRACK_INTERVAL: Duration = Duration::from_secs(2);
/// A single line longer than this is dropped from the line callback (it still
/// reaches the tail) so a child that never prints a newline cannot exhaust
/// memory.
const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// A command as an argument vector: never passed through a shell.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    /// Added on top of the inherited environment, or the whole environment
    /// when `env_clear` is set.
    pub env: Vec<(String, String)>,
    /// Start from an empty environment instead of this process's own.
    pub env_clear: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyOutput {
    /// `None` when the child was killed by a signal or never reported.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// The last `tail_bytes` of output, lossily decoded.
    pub tail: String,
    /// Pid of the child, which is also its process-group id.
    pub pid: Option<u32>,
}

/// Called on the drain thread with each complete output line, without the
/// line terminator.
pub type LineCallback = Box<dyn FnMut(&str) + Send>;

fn io_err(context: &str, err: impl std::fmt::Display) -> PortError {
    PortError::Io(format!("{context}: {err}"))
}

/// Runs `spec` to completion or until `timeout`, then kills whatever is left
/// of its process tree. `tail_bytes` bounds the captured output.
pub fn run_pty(
    spec: &CommandSpec,
    timeout: Duration,
    tail_bytes: usize,
    on_line: Option<LineCallback>,
) -> PortResult<PtyOutput> {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 200,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| io_err("openpty", e))?;

    let mut builder = CommandBuilder::new(&spec.program);
    builder.args(&spec.args);
    if let Some(cwd) = &spec.cwd {
        builder.cwd(cwd);
    }
    if spec.env_clear {
        builder.env_clear();
    }
    for (key, value) in &spec.env {
        builder.env(key, value);
    }
    let mut child = pair
        .slave
        .spawn_command(builder)
        .map_err(|e| io_err(&format!("spawn {}", spec.program), e))?;
    // The parent keeps its copy of the slave open until the child is gone.
    // On macOS the last close of the slave discards output the master has not
    // read yet, so a child that prints and exits at once (`echo`) would lose
    // its output if that close happened inside the child's exit.
    let slave = pair.slave;
    let pid = child.process_id();
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| io_err("pty reader", e))?;

    let tail = Arc::new(Mutex::new(Tail::new(tail_bytes)));
    let (drained_tx, drained_rx) = mpsc::channel::<()>();
    let drain_tail = Arc::clone(&tail);
    thread::spawn(move || {
        drain(reader, &drain_tail, on_line);
        // The receiver is gone only when the run gave up waiting for EOF.
        let _ = drained_tx.send(());
    });

    let (exit_tx, exit_rx) = mpsc::channel();
    thread::spawn(move || {
        // The receiver outlives this thread except when the run gave up on a
        // child that survived SIGKILL; there is nobody left to tell then.
        let _ = exit_tx.send(child.wait());
    });

    let tracker = pid.map(|pid| Arc::new(Mutex::new(ProcessTracker::new(pid))));
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let tracking = tracker.clone().map(|tracker| {
        thread::spawn(move || {
            loop {
                observe(&tracker);
                match stop_rx.recv_timeout(TRACK_INTERVAL) {
                    Err(RecvTimeoutError::Timeout) => {}
                    _ => break,
                }
            }
        })
    });

    let mut timed_out = false;
    let mut status = match exit_rx.recv_timeout(timeout) {
        Ok(status) => Some(status),
        Err(_) => {
            timed_out = true;
            None
        }
    };
    drop(stop_tx);
    if let Some(handle) = tracking {
        // A panic in the sampling thread only loses samples.
        let _ = handle.join();
    }

    if let Some(pid) = pid {
        if timed_out {
            // Sample once more while the tree is still attached to its root.
            if let Some(tracker) = &tracker {
                observe(tracker);
            }
            signal_group(pid, Signal::Term);
            status = exit_rx.recv_timeout(TERM_GRACE).ok();
        }
        // Always: on a normal exit this removes background jobs the child
        // left in its group; after SIGTERM it removes whatever ignored it.
        signal_group(pid, Signal::Kill);
        if let Some(tracker) = &tracker {
            // Descendants that moved to another group or session. A failed
            // `ps` here leaves them to the run-id scan of `reaper::reap`.
            let _ = tracker
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .kill_survivors();
        }
        if status.is_none() {
            status = exit_rx.recv_timeout(TERM_GRACE).ok();
        }
    }

    // Closing our copy lets the reader reach EOF after it has taken what the
    // child wrote. The close runs on its own thread because the kernel may
    // hold it until the pending output has been read.
    thread::spawn(move || drop(slave));
    // EOF arrives once every holder of the slave side is dead. A process that
    // escaped the kills above could hold it open forever, hence the bound; the
    // drain thread then stays parked on the read and the tail is cut here.
    let _ = drained_rx.recv_timeout(DRAIN_GRACE);
    drop(pair.master);

    let exit_code = match status {
        Some(Ok(status)) if status.signal().is_none() => i32::try_from(status.exit_code()).ok(),
        _ => None,
    };
    let tail = tail
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .to_string_lossy();
    Ok(PtyOutput {
        exit_code,
        timed_out,
        tail,
        pid,
    })
}

fn observe(tracker: &Mutex<ProcessTracker>) {
    // A failed sample (ps unavailable or slow) only narrows what the tracker
    // can find later; the group kill does not depend on it.
    let _ = tracker
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .observe();
}

fn signal_group(pgid: u32, signal: Signal) {
    // "No such process" is the normal answer when the group is already gone,
    // and there is no further fallback if /bin/kill itself fails.
    let _ = reaper::kill_group(pgid, signal);
}

/// Bounded buffer of the most recent output bytes.
struct Tail {
    bytes: VecDeque<u8>,
    cap: usize,
}

impl Tail {
    fn new(cap: usize) -> Self {
        Tail {
            bytes: VecDeque::new(),
            cap,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        let chunk = &chunk[chunk.len().saturating_sub(self.cap)..];
        let overflow = (self.bytes.len() + chunk.len()).saturating_sub(self.cap);
        self.bytes.drain(..overflow);
        self.bytes.extend(chunk);
    }

    fn to_string_lossy(&self) -> String {
        let (a, b) = self.bytes.as_slices();
        let mut all = Vec::with_capacity(a.len() + b.len());
        all.extend_from_slice(a);
        all.extend_from_slice(b);
        String::from_utf8_lossy(&all).into_owned()
    }
}

fn drain(mut reader: Box<dyn Read + Send>, tail: &Mutex<Tail>, mut on_line: Option<LineCallback>) {
    let mut buf = [0u8; 8192];
    let mut line: Vec<u8> = Vec::new();
    let mut line_overflowed = false;
    loop {
        let n = match reader.read(&mut buf) {
            // macOS reports the closed slave as EIO rather than a clean EOF;
            // either way there is nothing more to read.
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let chunk = &buf[..n];
        tail.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(chunk);
        let Some(callback) = on_line.as_mut() else {
            continue;
        };
        for &byte in chunk {
            if byte == b'\n' {
                if !line_overflowed {
                    emit(callback, &line);
                }
                line.clear();
                line_overflowed = false;
            } else if line.len() < MAX_LINE_BYTES {
                line.push(byte);
            } else {
                line_overflowed = true;
            }
        }
    }
    if let Some(callback) = on_line.as_mut()
        && !line.is_empty()
        && !line_overflowed
    {
        emit(callback, &line);
    }
}

fn emit(callback: &mut LineCallback, line: &[u8]) {
    // The tty line discipline turns "\n" into "\r\n".
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    callback(&String::from_utf8_lossy(line));
}
