//! Child-process plumbing shared by the command runner and by git calls
//! whose output can exceed a pipe buffer.

use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use aaf_fp::output::{self, Exited};

use super::GateError;

const POLL: Duration = Duration::from_millis(5);
/// Many children (a git call) end within a few milliseconds: while one is
/// this young its exit is checked every [`FAST_POLL`] instead of [`POLL`].
const FAST_POLL_FOR: Duration = Duration::from_millis(50);
const FAST_POLL: Duration = Duration::from_millis(1);
/// Longest wait for the output once the child is gone: only another holder
/// of the stream that goes on writing takes that long.
const DRAIN_LIMIT: Duration = Duration::from_secs(30);

pub(crate) struct Captured {
    pub status: ExitStatus,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// False when output still kept arriving [`DRAIN_LIMIT`] after the end.
    pub complete: bool,
    pub output: Vec<u8>,
    /// Time the child was running; time spent stopped is left out.
    pub duration: Duration,
    /// Time the child's group was stopped by its supervisor.
    pub paused: Duration,
}

/// Watches a running child. The load watchdog implements it to sample the
/// group's memory and to stop and continue it under memory pressure.
pub(crate) trait Supervise {
    /// Called on every poll with the child's process group and the time
    /// since it started. Returns true while the group is stopped.
    fn tick(&mut self, pgid: u32, elapsed: Duration) -> bool;

    /// Called once the timeout has passed, right before the group is
    /// killed: the last moment its helpers in other groups are still below
    /// the command, where a look at the process tree finds them.
    fn before_kill(&mut self, _pgid: u32) {}
}

struct Unsupervised;

impl Supervise for Unsupervised {
    fn tick(&mut self, _pgid: u32, _elapsed: Duration) -> bool {
        false
    }
}

struct Sink {
    data: Vec<u8>,
    /// Keep only this many trailing bytes; `None` keeps everything.
    cap: Option<usize>,
}

impl Sink {
    fn push(&mut self, chunk: &[u8]) {
        self.data.extend_from_slice(chunk);
        if let Some(cap) = self.cap
            && self.data.len() > cap.saturating_mul(2)
        {
            let cut = self.data.len() - cap;
            self.data.drain(..cut);
        }
    }

    fn take(&mut self) -> Vec<u8> {
        if let Some(cap) = self.cap
            && self.data.len() > cap
        {
            let cut = self.data.len() - cap;
            self.data.drain(..cut);
        }
        std::mem::take(&mut self.data)
    }
}

fn drain(
    mut pipe: impl Read + Send + 'static,
    sink: Arc<Mutex<Sink>>,
    done: mpsc::Sender<()>,
    stop: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            match pipe.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    // A poisoned lock means another reader panicked; the
                    // caller then sees whatever was collected before.
                    if let Ok(mut sink) = sink.lock() {
                        sink.push(&buf[..n]);
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                // The pipe is gone; nothing more can arrive.
                Err(_) => break,
            }
        }
        // The receiver may have stopped waiting after the grace period.
        let _ = done.send(());
    });
}

/// Longest time [`kill_group`] goes on killing a group that still has members.
const KILL_LIMIT: Duration = Duration::from_secs(30);
/// Pause between two kills of a group that still has members.
const KILL_AGAIN: Duration = Duration::from_millis(10);

/// `/bin/kill <args>`; true when it exited 0.
fn kill(args: &[&str]) -> bool {
    Command::new("/bin/kill")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// True when process group `pgid` still has at least one non-zombie member.
///
/// On macOS, `kill -0 -- -<pgid>` already reports empty once only an unreaped
/// zombie leader remains; on Linux it still succeeds until the leader is
/// reaped. Checking `ps -A -o pgid=,stat=` after `kill -0` succeeds avoids
/// spinning for the full [`KILL_LIMIT`] against a zombie leader on Linux,
/// while failing safe (returning `true`) if `ps` cannot run.
fn group_has_live_member(pgid: u32) -> bool {
    let group = format!("-{pgid}");
    if !kill(&["-0", "--", &group]) {
        return false;
    }
    let Ok(out) = Command::new("/bin/ps")
        .args(["-A", "-o", "pgid=,stat="])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return true;
    };
    if !out.status.success() {
        return true;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    if text.trim().is_empty() {
        return true;
    }
    let target = pgid.to_string();
    text.lines().any(|line| {
        let mut cols = line.split_whitespace();
        let Some(g) = cols.next() else {
            return false;
        };
        let Some(stat) = cols.next() else {
            return false;
        };
        g == target && !stat.starts_with('Z')
    })
}

/// SIGKILL to every process in the group led by `pgid`, again and again
/// until no live (non-zombie) member remains, for at most `limit`. False when
/// live members were still there at the end.
///
/// One SIGKILL is not enough on macOS: a child being forked at that instant
/// is created after the signal went out, keeps the group id and runs on
/// (measured with a shell that forks in a loop). Called before the leader is
/// reaped, so the group id cannot belong to anyone else yet.
fn kill_group_until_empty(pgid: u32, limit: Duration) -> bool {
    let group = format!("-{pgid}");
    let end = Instant::now() + limit;
    loop {
        // A non-zero exit is "no such process": the group ended by itself.
        kill(&["-KILL", "--", &group]);
        if !group_has_live_member(pgid) {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        thread::sleep(KILL_AGAIN);
    }
}

/// [`kill_group_until_empty`] with the gate's bound.
fn kill_group(pgid: u32) -> bool {
    kill_group_until_empty(pgid, KILL_LIMIT)
}

/// The line a captured log gets when its group could not be emptied.
fn unkilled_note(pgid: u32) -> String {
    format!(
        "\n[aaf] process group {pgid} still had members after {}s of SIGKILL; \
         they may still be running\n",
        KILL_LIMIT.as_secs()
    )
}

/// Runs `cmd` in its own process group with a deadline, draining its output
/// on threads so a chatty child cannot fill the pipe and stall.
pub(crate) fn capture(
    cmd: Command,
    timeout: Duration,
    cap: Option<usize>,
    with_stderr: bool,
) -> io::Result<Captured> {
    capture_supervised(cmd, timeout, cap, with_stderr, &mut Unsupervised)
}

/// How a supervised child ended.
pub(crate) struct Waited {
    pub status: ExitStatus,
    pub timed_out: bool,
    /// Time the child was running; time spent stopped is left out.
    pub duration: Duration,
    /// Time the child's group was stopped by its supervisor.
    pub paused: Duration,
}

/// The sleep before the next exit check of a child running for `elapsed`.
fn next_poll(elapsed: Duration) -> Duration {
    if elapsed < FAST_POLL_FOR {
        FAST_POLL
    } else {
        POLL
    }
}

/// Waits for `child`, the leader of its own process group, under
/// `supervisor`; kills the group once it has run for `timeout`. Besides how
/// it ended: the group, when the timeout kill gave up with members left.
fn wait_supervised(
    child: &mut Child,
    started: Instant,
    timeout: Duration,
    supervisor: &mut dyn Supervise,
) -> io::Result<(Waited, Option<u32>)> {
    let mut timed_out = false;
    let mut unkilled = None;
    let mut paused = false;
    let mut paused_total = Duration::ZERO;
    let mut last = started;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(err) => {
                // The wait error is what the caller gets; a group that would
                // not empty goes into its text, the only place left to say so.
                if kill_group(child.id()) {
                    return Err(err);
                }
                let note = unkilled_note(child.id());
                return Err(io::Error::new(
                    err.kind(),
                    format!("{err}; {}", note.trim()),
                ));
            }
        }
        let now = Instant::now();
        if paused {
            paused_total += now - last;
        }
        last = now;
        paused = supervisor.tick(child.id(), now - started);
        if !paused && (now - started).saturating_sub(paused_total) >= timeout {
            timed_out = true;
            supervisor.before_kill(child.id());
            // Kill before reaping: the pid still names our group.
            if !kill_group(child.id()) {
                unkilled = Some(child.id());
            }
            break child.wait()?;
        }
        thread::sleep(next_poll(now - started));
    };
    let end = Instant::now();
    if paused {
        paused_total += end - last;
    }
    let waited = Waited {
        status,
        timed_out,
        duration: (end - started).saturating_sub(paused_total),
        paused: paused_total,
    };
    Ok((waited, unkilled))
}

/// Runs `cmd` in its own process group under `supervisor` with the output
/// streams it was given (the caller's own, unless it set others): nothing
/// is captured. For a command whose output belongs to the user. Besides how
/// it ended: the group, when the timeout kill gave up with members left
/// (there is no log to put the line `capture_supervised` writes; the caller
/// reports it).
pub(crate) fn run_supervised(
    mut cmd: Command,
    timeout: Duration,
    supervisor: &mut dyn Supervise,
) -> io::Result<(Waited, Option<u32>)> {
    cmd.process_group(0);
    let started = Instant::now();
    let mut child = cmd.spawn()?;
    wait_supervised(&mut child, started, timeout, supervisor)
}

/// [`capture`] with a supervisor. Time during which the supervisor reports
/// the group stopped is not charged to `timeout`: a command paused to
/// protect the machine must not be killed for the pause.
pub(crate) fn capture_supervised(
    mut cmd: Command,
    timeout: Duration,
    cap: Option<usize>,
    with_stderr: bool,
    supervisor: &mut dyn Supervise,
) -> io::Result<Captured> {
    cmd.stdin(Stdio::null()).process_group(0);
    // Set once the child is reaped (or on an early return): from then on the
    // reader ends when nothing the child wrote is left, however long another
    // copy of the stream stays open ([`aaf_fp::output`]).
    let exited = Exited::new();
    // With stderr, both streams go into ONE stream, as `2>&1` would: the log
    // keeps the order the command wrote in. Two streams read by two threads
    // interleave by scheduling, and under load the late lines of one stream
    // pushed the last line of the other out of the kept tail.
    let reader = if with_stderr {
        let (stdout, stderr, reader) = output::merged_output(&exited)?;
        cmd.stdout(stdout).stderr(stderr);
        reader
    } else {
        let (stdout, reader) = output::output(&exited)?;
        cmd.stdout(stdout).stderr(Stdio::null());
        reader
    };
    let started = Instant::now();
    let mut child = cmd.spawn()?;
    // The command builder holds this process's copies of the write end;
    // while they are open the reader never sees the end of the output.
    drop(cmd);
    let sink = Arc::new(Mutex::new(Sink {
        data: Vec::new(),
        cap,
    }));
    let (done_tx, done_rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    drain(reader, Arc::clone(&sink), done_tx, Arc::clone(&stop));

    let (
        Waited {
            status,
            timed_out,
            duration,
            paused: paused_total,
        },
        unkilled,
    ) = match wait_supervised(&mut child, started, timeout, supervisor) {
        Ok(res) => res,
        Err(err) => {
            stop.store(true, Ordering::Relaxed);
            return Err(err);
        }
    };
    exited.mark();

    // The reader now ends once it has read everything the child wrote, even
    // while a detached grandchild (or a copy leaked into another child)
    // keeps the stream open. Only a holder that goes on writing keeps it
    // going, up to DRAIN_LIMIT; a reader starved on a loaded machine loses
    // nothing, what it has not read stays queued.
    let complete = match done_rx.recv_timeout(DRAIN_LIMIT) {
        // Sent, or the sender is gone: nothing more will arrive.
        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => true,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            stop.store(true, Ordering::Relaxed);
            false
        }
    };
    let mut output = sink.lock().map(|mut s| s.take()).unwrap_or_default();
    // The log is where a person reads what the timeout left behind.
    if let Some(pgid) = unkilled {
        output.extend_from_slice(unkilled_note(pgid).as_bytes());
    }
    Ok(Captured {
        status,
        exit_code: status.code(),
        timed_out,
        complete,
        output,
        duration,
        paused: paused_total,
    })
}

/// Like `aaf_fp::git` but returns the full, untrimmed stdout and reads it
/// while the child runs, so output larger than a pipe buffer (a diff, a file
/// list) cannot stall until the timeout.
pub(crate) fn git_bytes(
    repo: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<Vec<u8>, GateError> {
    // The git looked up once at start, outside the repository: a `git` the
    // task put in a `PATH` directory would otherwise run with the gate's
    // rights.
    let git = aaf_fp::program("git").map_err(|err| GateError::Git(err.to_string()))?;
    let mut cmd = Command::new(git);
    // No auto gc in any git command of aaf (docs/PLAN.md C.8): a repack must
    // not start inside a call that has a deadline. No fsmonitor either: git
    // checks a repository embedded in the work tree with a child `git status`
    // that would start the `core.fsmonitor` the agent wrote into it; `-c`
    // reaches those child gits through GIT_CONFIG_PARAMETERS.
    cmd.arg("-C")
        .arg(repo)
        .args(["-c", "gc.auto=0", "-c", "core.fsmonitor=false"])
        .args(args);
    for var in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"] {
        cmd.env_remove(var);
    }
    // The git command, past the caller's own `-c <name>=<value>` options.
    let mut rest = args;
    while let ["-c", _, tail @ ..] = rest {
        rest = tail;
    }
    let label = rest.first().copied().unwrap_or("");
    let done = capture(cmd, timeout, None, false)?;
    if done.timed_out {
        return Err(GateError::Git(format!("{label}: timed out")));
    }
    if done.exit_code != Some(0) {
        return Err(GateError::Git(format!(
            "{label}: exit {:?}",
            done.exit_code
        )));
    }
    if !done.complete {
        return Err(GateError::Git(format!(
            "{label}: output still arriving after git exited"
        )));
    }
    Ok(done.output)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    struct EndlessReader;
    impl Read for EndlessReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            thread::sleep(Duration::from_millis(5));
            buf[0] = b'x';
            Ok(1)
        }
    }

    #[test]
    fn drain_exits_when_stop_is_set() {
        let sink = Arc::new(Mutex::new(Sink {
            data: Vec::new(),
            cap: Some(64),
        }));
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        drain(EndlessReader, sink, tx, Arc::clone(&stop));
        stop.store(true, Ordering::Relaxed);
        assert!(rx.recv_timeout(Duration::from_secs(1)).is_ok());
    }

    #[test]
    fn next_poll_is_fast_while_young() {
        let ms = Duration::from_millis;
        for elapsed in [0, 1, 8, 49] {
            assert_eq!(next_poll(ms(elapsed)), ms(1));
        }
        for elapsed in [50, 51, 5_000] {
            assert_eq!(next_poll(ms(elapsed)), POLL);
        }
    }
}
