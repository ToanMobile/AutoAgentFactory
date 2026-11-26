//! Child-process plumbing shared by the command runner, throttle watchdog,
//! proof capture/flows, and git calls whose output can exceed a pipe buffer.

use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use aaf_fp::output::{self, Exited};

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
    started: Instant,
    last_output_ms: Arc<std::sync::atomic::AtomicU64>,
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
                    let ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
                    last_output_ms.store(ms.max(1), Ordering::Relaxed);
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
/// Pause before the second look that confirms a killed group is empty.
const CONFIRM_EMPTY: Duration = Duration::from_millis(50);

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
            // A child forked at the instant of the snapshot can be missed by
            // it (seen on Linux CI): empty only when a second look, after
            // one more kill, still finds no member.
            thread::sleep(CONFIRM_EMPTY);
            kill(&["-KILL", "--", &group]);
            if !group_has_live_member(pgid) {
                return true;
            }
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

/// After the leader was reaped: SIGKILL the members it left in its group,
/// checking before every kill that the group still has a live member. A
/// live member keeps the group id in use, so it cannot name anyone else;
/// once the group is empty nothing is sent (no blind confirm-kill). The
/// only remaining race is between the `ps` check seeing a live member and
/// the `kill` immediately after, which would require pid wraparound in a
/// few milliseconds (same policy as `executors/pty.rs`).
fn kill_left_members(pgid: u32, limit: Duration) -> bool {
    let group = format!("-{pgid}");
    let end = Instant::now() + limit;
    loop {
        if !group_has_live_member(pgid) {
            return true;
        }
        kill(&["-KILL", "--", &group]);
        if Instant::now() >= end {
            return !group_has_live_member(pgid);
        }
        thread::sleep(KILL_AGAIN);
    }
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
#[allow(dead_code)]
pub(crate) fn capture(
    cmd: Command,
    timeout: Duration,
    cap: Option<usize>,
    with_stderr: bool,
) -> io::Result<Captured> {
    capture_with_idle(cmd, timeout, None, cap, with_stderr)
}

/// [`capture`] with an optional output-silence deadline.
pub(crate) fn capture_with_idle(
    cmd: Command,
    timeout: Duration,
    idle_timeout: Option<Duration>,
    cap: Option<usize>,
    with_stderr: bool,
) -> io::Result<Captured> {
    capture_supervised_with_idle(
        cmd,
        timeout,
        idle_timeout,
        cap,
        with_stderr,
        &mut Unsupervised,
    )
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

fn idle_killed_note(limit: Duration) -> String {
    format!(
        "\n[aaf] command produced no output for {}s (idle_timeout_secs) and was terminated\n",
        limit.as_secs()
    )
}

/// Waits for `child`, the leader of its own process group, under
/// `supervisor`; kills the group once it has run for `timeout` (or stays silent
/// for `idle_timeout` unpaused), and on a normal exit kills the members the
/// leader left in its group.
fn wait_supervised(
    child: &mut Child,
    started: Instant,
    timeout: Duration,
    idle: Option<(Duration, &std::sync::atomic::AtomicU64)>,
    supervisor: &mut dyn Supervise,
) -> io::Result<(Waited, Option<u32>, Option<Duration>)> {
    let mut timed_out = false;
    let mut idle_killed = None;
    let mut unkilled = None;
    let mut paused = false;
    let mut paused_total = Duration::ZERO;
    let mut last = started;
    let mut seen_output_ms = 0u64;
    let mut idle_elapsed = Duration::ZERO;
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
        let delta = now.saturating_duration_since(last);
        if paused {
            paused_total += delta;
        }
        if let Some((_, tracker)) = idle {
            let current_out = tracker.load(Ordering::Relaxed);
            if current_out != seen_output_ms {
                seen_output_ms = current_out;
                idle_elapsed = Duration::ZERO;
            } else if !paused {
                idle_elapsed = idle_elapsed.saturating_add(delta);
            }
        }
        last = now;
        paused = supervisor.tick(child.id(), now - started);
        let hit_wall = (now - started).saturating_sub(paused_total) >= timeout;
        let hit_idle =
            idle.and_then(|(lim, _)| (!lim.is_zero() && idle_elapsed >= lim).then_some(lim));
        if !paused && (hit_wall || hit_idle.is_some()) {
            timed_out = true;
            if !hit_wall {
                idle_killed = hit_idle;
            }
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
    // Normal exit: the leader is reaped, so `supervisor.before_kill` (which
    // is tied to timeouts) is not called; kill any background jobs still
    // living in the leader's group without charging cleanup to `duration`.
    if !timed_out && !kill_left_members(child.id(), KILL_LIMIT) {
        unkilled = Some(child.id());
    }
    Ok((waited, unkilled, idle_killed))
}

/// Runs `cmd` in its own process group under `supervisor` with the output
/// streams it was given (the caller's own, unless it set others): nothing
/// is captured. For a command whose output belongs to the user. Besides how
/// it ended: the group, when the timeout or normal-exit kill gave up with
/// members left (there is no log to put the line `capture_supervised`
/// writes; the caller reports it).
pub(crate) fn run_supervised(
    mut cmd: Command,
    timeout: Duration,
    supervisor: &mut dyn Supervise,
) -> io::Result<(Waited, Option<u32>)> {
    cmd.process_group(0);
    let started = Instant::now();
    let mut child = cmd.spawn()?;
    let (waited, unkilled, _) = wait_supervised(&mut child, started, timeout, None, supervisor)?;
    Ok((waited, unkilled))
}

/// [`capture`] with a supervisor. Time during which the supervisor reports
/// the group stopped is not charged to `timeout`: a command paused to
/// protect the machine must not be killed for the pause. On a normal exit,
/// kills the members the leader left in its group.
#[allow(dead_code)]
pub(crate) fn capture_supervised(
    cmd: Command,
    timeout: Duration,
    cap: Option<usize>,
    with_stderr: bool,
    supervisor: &mut dyn Supervise,
) -> io::Result<Captured> {
    capture_supervised_with_idle(cmd, timeout, None, cap, with_stderr, supervisor)
}

/// [`capture_supervised`] with an optional unpaused output-silence timeout.
pub(crate) fn capture_supervised_with_idle(
    mut cmd: Command,
    timeout: Duration,
    idle_timeout: Option<Duration>,
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
    let last_output_ms = Arc::new(std::sync::atomic::AtomicU64::new(0));
    drain(
        reader,
        Arc::clone(&sink),
        done_tx,
        Arc::clone(&stop),
        started,
        Arc::clone(&last_output_ms),
    );
    let idle = idle_timeout
        .filter(|d| !d.is_zero())
        .map(|d| (d, last_output_ms.as_ref()));

    let (
        Waited {
            status,
            timed_out,
            duration,
            paused: paused_total,
        },
        unkilled,
        idle_killed,
    ) = match wait_supervised(&mut child, started, timeout, idle, supervisor) {
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
    if let Some(lim) = idle_killed {
        output.extend_from_slice(idle_killed_note(lim).as_bytes());
    }
    // The log is where a person reads what the timeout or a normal exit left behind.
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
pub(crate) fn git_bytes(repo: &Path, args: &[&str], timeout: Duration) -> Result<Vec<u8>, String> {
    // The git looked up once at start, outside the repository: a `git` the
    // task put in a `PATH` directory would otherwise run with the gate's
    // rights.
    let git = aaf_fp::program("git").map_err(|err| err.to_string())?;
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
    const SCRUBBED_GIT_ENV: [&str; 6] = [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_PREFIX",
    ];
    for var in SCRUBBED_GIT_ENV {
        cmd.env_remove(var);
    }
    // The git command, past the caller's own `-c <name>=<value>` options.
    let mut rest = args;
    while let ["-c", _, tail @ ..] = rest {
        rest = tail;
    }
    let label = rest.first().copied().unwrap_or("");
    let done = capture(cmd, timeout, None, false).map_err(|err| format!("{label}: {err}"))?;
    if done.timed_out {
        return Err(format!("{label}: timed out"));
    }
    if done.exit_code != Some(0) {
        return Err(format!("{label}: exit {:?}", done.exit_code));
    }
    if !done.complete {
        return Err(format!("{label}: output still arriving after git exited"));
    }
    Ok(done.output)
}
