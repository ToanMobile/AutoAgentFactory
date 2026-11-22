//! What happens while a heavy command runs: low priority, memory sampling,
//! and stop/continue of its process group under critical memory pressure.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use super::daemons::{self, BuildDaemon};
use super::orphans::{self, PauseRecord};
use super::probe::{self, GroupState, LoadProbe, PROBE_TIMEOUT, PS, Pressure, ProcId, Unreadable};
use super::relay::SignalRelay;
use super::slots::SlotGuard;
use super::{Progress, Sleeper};
use crate::gate::proc::Supervise;

/// Niceness added to a heavy command.
pub const NICE_INCREMENT: &str = "10";

/// Where the priority tools are, when present.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PriorityTools {
    pub nice: Option<PathBuf>,
    /// macOS only: `taskpolicy -b` also lowers I/O priority and keeps the
    /// work off the performance cores.
    pub taskpolicy: Option<PathBuf>,
}

impl PriorityTools {
    /// `exists` and `macos` are parameters so both branches are testable.
    pub fn locate(exists: &dyn Fn(&Path) -> bool, macos: bool) -> PriorityTools {
        let find = |candidates: &[&str]| {
            candidates
                .iter()
                .map(PathBuf::from)
                .find(|path| exists(path))
        };
        PriorityTools {
            nice: find(&["/usr/bin/nice", "/bin/nice"]),
            taskpolicy: if macos {
                find(&["/usr/sbin/taskpolicy", "/usr/bin/taskpolicy"])
            } else {
                None
            },
        }
    }

    pub fn detect() -> PriorityTools {
        PriorityTools::locate(&|path| path.exists(), cfg!(target_os = "macos"))
    }
}

/// Argument vector that runs `command` through `/bin/sh -c`, at low priority
/// when asked and the tools exist. Both wrappers `exec` their program, so the
/// spawned pid stays the leader of the command's process group.
///
/// The shell is named by its path: a bare `sh` is looked up through `PATH`
/// (by `aaf` or by the wrappers), where a `sh` the task committed into the
/// repository could come first and would run with the gate's rights.
pub fn heavy_argv(command: &str, low_priority: bool, tools: &PriorityTools) -> Vec<OsString> {
    let shell: [OsString; 3] = ["/bin/sh".into(), "-c".into(), command.into()];
    priority_argv(&shell, low_priority, tools)
}

/// `program` behind the priority wrappers, when asked and the tools exist.
pub fn priority_argv(
    program: &[OsString],
    low_priority: bool,
    tools: &PriorityTools,
) -> Vec<OsString> {
    let mut argv: Vec<OsString> = Vec::new();
    if low_priority {
        if let Some(taskpolicy) = &tools.taskpolicy {
            argv.push(taskpolicy.into());
            argv.push("-b".into());
        }
        if let Some(nice) = &tools.nice {
            argv.push(nice.into());
            argv.push("-n".into());
            argv.push(NICE_INCREMENT.into());
        }
    }
    argv.extend(program.iter().cloned());
    argv
}

/// Acts on a whole process group. Faked in tests.
pub trait GroupControl: Send + Sync {
    /// Resident memory of the group in bytes; `None` when unreadable.
    fn rss_bytes(&self, pgid: u32) -> Option<u64>;
    /// SIGSTOP the group. False when the signal could not be sent.
    fn stop(&self, pgid: u32) -> bool;
    /// SIGCONT the group.
    fn resume(&self, pgid: u32) -> bool;
    /// Whether the group still exists, is stopped, and when its leader
    /// started; `None` when it has no process or cannot be read. An orphan
    /// is only continued on the strength of this, so an implementation that
    /// cannot tell never continues one.
    fn group_state(&self, _pgid: u32) -> Option<GroupState> {
        None
    }
    /// [`GroupControl::group_state`] that tells a group that is not there
    /// (`Ok(None)`) from a process table that could not be read.
    fn read_group_state(&self, pgid: u32) -> Result<Option<GroupState>, Unreadable> {
        Ok(self.group_state(pgid))
    }
    /// The command's whole process tree ([`probe::tree_members`]) and its
    /// resident memory. `None` when it cannot be read; the caller then
    /// falls back to the group alone.
    fn tree(&self, _pgid: u32, _known: &[ProcId]) -> Option<TreeSample> {
        None
    }
    /// Sends `signal` to every process of `members` that is still the same
    /// process (pid and start time), each by its pid. Returns how many.
    fn signal_members(&self, _signal: &str, _members: &[ProcId]) -> usize {
        0
    }
    /// Gradle and Kotlin daemons running now.
    fn build_daemons(&self) -> Vec<BuildDaemon> {
        Vec::new()
    }
    /// Working directory of a process; `None` when it cannot be read.
    fn process_cwd(&self, _pid: u32) -> Option<PathBuf> {
        None
    }
}

/// One sample of a command's process tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeSample {
    pub rss_bytes: u64,
    pub members: Vec<ProcId>,
}

/// `ps` for memory and `/bin/kill` for signals (`libc::kill` needs `unsafe`).
#[derive(Debug, Clone, Copy)]
pub struct SystemGroup;

fn tree_rows() -> Option<Vec<probe::ProcRow>> {
    let listing = probe::tool_output(PS, &["-A", "-o", probe::PS_TREE_COLUMNS])?;
    Some(probe::parse_tree(&listing))
}

/// How many times a look at the process table that has to happen (the
/// tree a timeout ends, the check before its signal) asks `ps`. Under load
/// one `ps` was seen to run past [`PROBE_TIMEOUT`] while dozens of others
/// ran; the next one usually answers.
pub(crate) const SIGNAL_READS: usize = 3;

/// The first answer of `read` in [`SIGNAL_READS`] tries.
fn read_again(read: &dyn Fn() -> Option<Vec<probe::ProcRow>>) -> Option<Vec<probe::ProcRow>> {
    (0..SIGNAL_READS).find_map(|_| read())
}

/// [`SystemGroup::signal_members`] with the process table (`read`) and
/// `kill` (`send`) passed in.
pub(crate) fn signal_verified(
    read: &dyn Fn() -> Option<Vec<probe::ProcRow>>,
    send: &dyn Fn(&str, &[String]),
    signal: &str,
    members: &[ProcId],
) -> usize {
    // Read again right before the signal: only a pid that still has the
    // start time it was seen with is the process that was seen.
    let pids: Vec<String> = match read_again(read) {
        Some(rows) => rows
            .iter()
            .filter(|row| row.id.pid > 1 && members.contains(&row.id))
            .map(|row| row.id.pid.to_string())
            .collect(),
        // No answer at all: skipping the signal left a helper of a timed
        // out command running for good, holding the memory the next command
        // is admitted on. The members are signalled as the last sample saw
        // them, seconds ago (the timeout reads the tree once more right
        // before its kill); a pid handed to another process since then
        // would need the pid space to wrap within those seconds.
        None => members
            .iter()
            .filter(|member| member.pid > 1)
            .map(|member| member.pid.to_string())
            .collect(),
    };
    if pids.is_empty() {
        return 0;
    }
    send(signal, &pids);
    pids.len()
}

/// `/bin/kill <signal> <pids>`.
fn send_signal(signal: &str, pids: &[String]) {
    let mut command = Command::new("/bin/kill");
    command
        .arg(signal)
        .args(pids)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // A process that ended in between makes kill exit non-zero; the
    // others got the signal all the same.
    let _ = crate::process::run_with_timeout(command, PROBE_TIMEOUT, false);
}

fn signal_group(signal: &str, pgid: u32) -> bool {
    let mut command = Command::new("/bin/kill");
    command
        .args([signal, "--", &format!("-{pgid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::process::run_with_timeout(command, PROBE_TIMEOUT, false)
        .ok()
        .and_then(|finished| finished.status)
        .is_some_and(|status| status.success())
}

impl GroupControl for SystemGroup {
    fn rss_bytes(&self, pgid: u32) -> Option<u64> {
        // Listing every process and filtering here behaves the same on
        // macOS and Linux; their `ps -g` flags do not.
        let listing = probe::tool_output(PS, &["-A", "-o", "pgid=,rss="])?;
        probe::sum_group_rss(&listing, pgid)
    }

    fn tree(&self, pgid: u32, known: &[ProcId]) -> Option<TreeSample> {
        let rows = tree_rows()?;
        let members = probe::tree_members(&rows, pgid, known);
        let rss_bytes = probe::tree_rss(&rows, &members)?;
        Some(TreeSample { rss_bytes, members })
    }

    fn signal_members(&self, signal: &str, members: &[ProcId]) -> usize {
        signal_verified(&tree_rows, &send_signal, signal, members)
    }

    fn stop(&self, pgid: u32) -> bool {
        signal_group("-STOP", pgid)
    }

    fn resume(&self, pgid: u32) -> bool {
        signal_group("-CONT", pgid)
    }

    fn group_state(&self, pgid: u32) -> Option<GroupState> {
        self.read_group_state(pgid).ok().flatten()
    }

    fn read_group_state(&self, pgid: u32) -> Result<Option<GroupState>, Unreadable> {
        let listing =
            probe::tool_output(PS, &["-A", "-o", probe::PS_GROUP_COLUMNS]).ok_or(Unreadable)?;
        Ok(probe::group_state(&listing, pgid))
    }

    fn build_daemons(&self) -> Vec<BuildDaemon> {
        daemons::list()
    }

    fn process_cwd(&self, pid: u32) -> Option<PathBuf> {
        // Linux has it as a link; macOS only through `lsof`.
        if let Ok(cwd) = std::fs::read_link(format!("/proc/{pid}/cwd")) {
            return Some(cwd);
        }
        let pid = pid.to_string();
        let listing = probe::tool_output(LSOF, &["-a", "-p", &pid, "-d", "cwd", "-Fn"])?;
        // Field output: one `n<path>` line for the one descriptor asked for.
        listing
            .lines()
            .find_map(|line| line.strip_prefix('n'))
            .map(PathBuf::from)
    }
}

const LSOF: &[&str] = &["/usr/sbin/lsof", "/usr/bin/lsof"];

/// Supervises one running heavy command.
///
/// Slots live in different processes, so each process decides for its own
/// command: the holder of slot 0 is never stopped, a holder of a later slot
/// stops its command while memory pressure is critical. A stopped command
/// moves to slot 0 as soon as that is free, so one command always runs.
pub(crate) struct Watchdog<'a> {
    pub probe: &'a dyn LoadProbe,
    pub control: &'a dyn GroupControl,
    pub progress: &'a dyn Fn(&Progress<'_>),
    pub data_dir: &'a Path,
    pub label: &'a str,
    pub slot: SlotGuard,
    pub sample_every: Duration,
    next_sample: Duration,
    paused: bool,
    /// Held while the group is stopped (see [`orphans`]).
    record: Option<PauseRecord>,
    pgid: Option<u32>,
    pub peak_rss_bytes: Option<u64>,
    /// The command's process tree as of the last sample.
    pub tracked: Vec<ProcId>,
    /// Passes a terminal's interrupt on to the command ([`super::relay`]).
    relay: Option<SignalRelay>,
}

impl<'a> Watchdog<'a> {
    pub fn new(
        probe: &'a dyn LoadProbe,
        control: &'a dyn GroupControl,
        progress: &'a dyn Fn(&Progress<'_>),
        data_dir: &'a Path,
        label: &'a str,
        slot: SlotGuard,
        sample_every: Duration,
    ) -> Watchdog<'a> {
        Watchdog {
            probe,
            control,
            progress,
            data_dir,
            label,
            slot,
            sample_every,
            next_sample: Duration::ZERO,
            paused: false,
            record: None,
            pgid: None,
            peak_rss_bytes: None,
            tracked: Vec::new(),
            relay: None,
        }
    }

    /// Stops the group. The record is written first, so a group stopped by
    /// an `aaf` that is killed right after is still found by the next run.
    fn stop(&mut self, pgid: u32) -> bool {
        // If the record cannot be written the stop still happens: easing
        // critical memory pressure now outweighs the rare orphan.
        // The leader's start time goes into the record: a later sweep must
        // not continue an unrelated group that got the same id.
        let started = self
            .control
            .group_state(pgid)
            .and_then(|state| state.leader_started);
        let record = orphans::record_pause(self.data_dir, pgid, started.as_deref()).ok();
        let stopped = self.control.stop(pgid);
        match (stopped, record) {
            (true, record) => self.record = record,
            (false, Some(record)) => record.clear(),
            (false, None) => {}
        }
        stopped
    }

    fn resume(&mut self, pgid: u32) {
        // A failed SIGCONT leaves the group stopped; keep reporting it paused
        // so the next sample tries again and no timeout is charged.
        if self.control.resume(pgid) {
            self.paused = false;
            if let Some(record) = self.record.take() {
                record.clear();
            }
            (self.progress)(&Progress::Resumed {
                label: self.label,
                slot: self.slot.index(),
            });
        }
    }

    /// Wakes the group if it is still stopped. Called once the command is
    /// over, so a group that outlived its leader is never left frozen.
    pub fn finish(&mut self) {
        if let (true, Some(pgid)) = (self.paused, self.pgid) {
            self.resume(pgid);
        }
        if let Some(relay) = self.relay.take() {
            relay.stop();
        }
    }
}

impl Supervise for Watchdog<'_> {
    fn tick(&mut self, pgid: u32, elapsed: Duration) -> bool {
        if elapsed < self.next_sample {
            return self.paused;
        }
        self.next_sample = elapsed + self.sample_every;
        if self.pgid != Some(pgid) {
            self.pgid = Some(pgid);
            // First, before the slower `ps` runs below: an interrupt that
            // arrives now already has somebody to pass it on. Without the
            // relay (no `/bin/sh`, no pipe) the command runs as before.
            self.relay = SignalRelay::start(pgid).ok();
            // From here on an admission to another slot can see how much
            // of this command's reservation is already in use.
            self.slot.record_pgid(pgid);
            // Then with the leader's start time (one `ps` run later): if
            // this process is killed, the record is how the next command
            // recognises the one left running.
            let started = self
                .control
                .group_state(pgid)
                .and_then(|state| state.leader_started);
            self.slot.record_group(pgid, started);
        }
        // The whole tree, not the group: a Unity or Gradle command keeps most
        // of its memory in helpers that run in groups of their own (measured
        // on a real Unity run: 1.29 GB in the group, 3.96 GB in the tree),
        // and the figure learned here decides what is admitted next to it.
        let rss = match self.control.tree(pgid, &self.tracked) {
            Some(sample) => {
                self.tracked = sample.members;
                Some(sample.rss_bytes)
            }
            None => self.control.rss_bytes(pgid),
        };
        // 0 bytes is no sample: a process that still runs has resident
        // pages, so the listing caught processes that had exited and wait to
        // be reaped (a command that ended right before this `ps`). Taken as
        // a figure it would let the command share the machine as if it
        // needed nothing, and push its real figures out of the stats.
        if let Some(rss) = rss.filter(|rss| *rss > 0) {
            self.peak_rss_bytes = Some(self.peak_rss_bytes.unwrap_or(0).max(rss));
        }
        if self.slot.index() == 0 {
            // Only reachable while paused after a promotion whose SIGCONT
            // failed: retry it.
            if self.paused {
                self.resume(pgid);
            }
            return self.paused;
        }
        let pressure = self.probe.pressure();
        // An unreadable slot directory counts as "slot 0 still taken".
        let promoted = (self.paused || pressure == Some(Pressure::Critical))
            && self
                .slot
                .promote(self.data_dir, &|pgid| self.control.group_state(pgid))
                .unwrap_or(false);
        if self.paused {
            // Unknown pressure is treated as normal, like before a run.
            if promoted || matches!(pressure, None | Some(Pressure::Normal)) {
                self.resume(pgid);
            }
        } else if pressure == Some(Pressure::Critical) && !promoted && self.stop(pgid) {
            self.paused = true;
            (self.progress)(&Progress::Paused {
                label: self.label,
                slot: self.slot.index(),
            });
        }
        self.paused
    }

    fn before_kill(&mut self, pgid: u32) {
        // Samples can all have gone unanswered (or come before a helper
        // started): the helpers the timeout ends after the group kill are
        // the ones known here. Once the group is gone, a helper reparented
        // to pid 1 can no longer be traced to the command.
        for _ in 0..SIGNAL_READS {
            if let Some(sample) = self.control.tree(pgid, &self.tracked) {
                self.tracked = sample.members;
                return;
            }
        }
    }
}

/// Real sleeps, for production.
#[derive(Debug, Clone, Copy)]
pub struct ThreadSleeper;

impl Sleeper for ThreadSleeper {
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}
