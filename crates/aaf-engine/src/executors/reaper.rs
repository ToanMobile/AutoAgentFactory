//! Finds and kills processes an agent run left behind.
//!
//! Three mechanisms, because no single one is complete on macOS:
//!
//! 1. [`kill_group`]: the run's root is a process-group leader, so one signal
//!    to the group reaches everything that did not change group. Complete for
//!    plain background jobs; misses anything that called `setsid`/`setpgid`.
//! 2. [`ProcessTracker`]: samples the descendant tree while the run is alive
//!    and remembers each `(pid, start time)`, so descendants are still known
//!    after their parents died and they were re-parented to pid 1.
//! 3. [`reap`]: scans `ps eww` for `AAF_RUN_ID=<run_id>` in the environment.
//!    Works across aaf restarts, but only for the same user, and (measured on
//!    macOS 27.2) `ps` shows no environment for Apple platform binaries such
//!    as `/bin/sh` or `/bin/sleep`; living descendants of a match are
//!    therefore killed with it.
//!
//! What still escapes all three: a process that leaves the group (a shell
//! started as its own group leader, as agent CLIs start their tool shells,
//! is enough), loses its parent between two tracker samples, and shows no
//! run id (`env -i`, or simply an Apple binary). Nothing visible to `ps`
//! ties such a process to the run. The one mark it cannot shed is the
//! Seatbelt sandbox it runs in: [`reap_in`] asks the kernel about it
//! (`sandbox_check`, the one module allowed `unsafe`, see
//! [`super::sandbox_check`]) through the run's [`RunMarks`].

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use aaf_domain::ports::{PortError, PortResult};

use super::capture;

const PS: &str = "/bin/ps";
const KILL: &str = "/bin/kill";
const PS_TIMEOUT: Duration = Duration::from_secs(10);
const KILL_TIMEOUT: Duration = Duration::from_secs(5);
const PS_READS: usize = 3;

/// Name of the environment variable every spawned agent carries.
pub const RUN_ID_ENV: &str = "AAF_RUN_ID";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Term,
    Kill,
}

impl Signal {
    fn flag(self) -> &'static str {
        match self {
            Signal::Term => "-TERM",
            Signal::Kill => "-KILL",
        }
    }
}

/// Two empty files that tell a run's sandbox from every other one: the
/// run's profile lets it write the data of `mark` and nothing else about
/// it, and nothing about `control`. The kernel answers "allowed" only for a
/// file that exists (measured on macOS 27.2: a missing path, even one the
/// profile lists, is "denied"), so aaf creates both before the agent starts,
/// in a directory the agent can neither read nor write (aaf's data
/// directory), and removes them after the reaper ran. A profile that lets
/// its process write everywhere answers "allowed" for `control` too and is
/// not taken for the run's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunMarks {
    pub mark: String,
    pub control: String,
}

impl RunMarks {
    /// The marks of `run_id` in `dir` (not created).
    pub fn at(dir: &std::path::Path, run_id: &str) -> PortResult<Self> {
        validate_run_id(run_id)?;
        let path = |name: String| dir.join(name).to_string_lossy().into_owned();
        Ok(Self {
            mark: path(format!("{run_id}.mark")),
            control: path(format!("{run_id}.control")),
        })
    }

    /// Creates `dir` (0700) and both files.
    pub fn create(&self) -> PortResult<()> {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let io = |err: std::io::Error| PortError::Io(format!("run marks: {err}"));
        if let Some(dir) = std::path::Path::new(&self.mark).parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(io)?;
        }
        for file in [&self.mark, &self.control] {
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(file)
                .map_err(io)?;
        }
        Ok(())
    }

    /// Removes both files; one already gone is not an error.
    pub fn remove(&self) -> PortResult<()> {
        for file in [&self.mark, &self.control] {
            match std::fs::remove_file(file) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(PortError::Io(format!("run marks: {err}"))),
            }
        }
        Ok(())
    }
}

/// A run id ends up in a `ps` text match, so only a conservative alphabet is
/// accepted.
pub fn validate_run_id(run_id: &str) -> PortResult<()> {
    let ok = !run_id.is_empty()
        && run_id.len() <= 128
        && run_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(PortError::Invalid(format!("run id {run_id:?}")))
    }
}

/// Signals every member of process group `pgid`. Returns whether the group
/// existed.
pub fn kill_group(pgid: u32, signal: Signal) -> PortResult<bool> {
    // `kill -- -1` addresses every process of the user and `-0` our own group.
    if pgid <= 1 {
        return Err(PortError::Invalid(format!("process group {pgid}")));
    }
    let target = format!("-{pgid}");
    let (code, _) = capture(KILL, &[signal.flag(), "--", &target], KILL_TIMEOUT)?;
    Ok(code == Some(0))
}

fn kill_pid(pid: u32, signal: Signal) -> PortResult<bool> {
    if pid <= 1 {
        return Err(PortError::Invalid(format!("pid {pid}")));
    }
    let (code, _) = capture(KILL, &[signal.flag(), &pid.to_string()], KILL_TIMEOUT)?;
    Ok(code == Some(0))
}

/// True while `pid` exists and is not a zombie.
pub fn is_alive(pid: u32) -> PortResult<bool> {
    let (_, out) = capture(PS, &["-o", "stat=", "-p", &pid.to_string()], PS_TIMEOUT)?;
    let stat = out.trim();
    Ok(!stat.is_empty() && !stat.starts_with('Z'))
}

/// Kills every process of this user that carries `AAF_RUN_ID=<run_id>`, plus
/// the living descendants of those. Returns the pids that were signalled.
pub fn reap(run_id: &str) -> PortResult<Vec<u32>> {
    reap_in(run_id, None)
}

/// [`reap`], plus every process the kernel places in the sandbox `marks`
/// belong to: what shows no run id (`env -i`, an Apple binary that left the
/// group and lost its parent) is still in the run's sandbox.
pub fn reap_in(run_id: &str, marks: Option<&RunMarks>) -> PortResult<Vec<u32>> {
    reap_with(run_id, marks, &system_ps)
}

/// One call of `ps` with these arguments: its exit code and stdout.
type Ps<'a> = &'a dyn Fn(&[&str]) -> PortResult<(Option<i32>, String)>;

fn system_ps(args: &[&str]) -> PortResult<(Option<i32>, String)> {
    capture(PS, args, PS_TIMEOUT)
}

/// The listing `ps` prints for `args`, asked up to `tries` times.
///
/// A run's cleanup cannot be done without it, and under load (seen at a
/// load average of ~75) one `ps` can go unanswered past [`PS_TIMEOUT`] while
/// the next one answers, as `throttle::watchdog::SIGNAL_READS` found for the
/// gate's timeout kill: the cleanup asks [`PS_READS`] times. A periodic
/// sample asks once; the next sample is its second try. The last error is
/// returned when none answers.
fn ps_listing(ps: Ps, args: &[&str], tries: usize) -> PortResult<String> {
    let mut last = PortError::Io("ps was not asked".to_string());
    for _ in 0..tries {
        match ps(args) {
            Ok((Some(0), listing)) => return Ok(listing),
            Ok((code, _)) => last = PortError::Io(format!("ps exited with {code:?}")),
            Err(err) => last = err,
        }
    }
    Err(last)
}

/// [`reap_in`] with `ps` passed in.
fn reap_with(run_id: &str, marks: Option<&RunMarks>, ps: Ps) -> PortResult<Vec<u32>> {
    validate_run_id(run_id)?;
    // `e` appends the environment to the command column; `ww` stops `ps`
    // truncating it.
    let listing = ps_listing(ps, &["eww", "-Ao", "pid=,ppid=,command="], PS_READS)?;
    let self_pid = std::process::id();
    let mut victims = select_victims(&listing, run_id, self_pid);
    if let Some(marks) = marks {
        let protected = self_and_ancestors(&listing, self_pid);
        for pid in listed_pids(&listing) {
            if !protected.contains(&pid) && !victims.contains(&pid) && in_run_sandbox(pid, marks) {
                victims.push(pid);
            }
        }
    }
    let mut killed = Vec::new();
    for pid in victims {
        if kill_pid(pid, Signal::Kill)? {
            killed.push(pid);
        }
    }
    Ok(killed)
}

/// Whether `pid` runs in the sandbox `marks` belong to: sandboxed, may write
/// the mark, and may not write the control. No answer from the kernel
/// (another user's process, one that just ended) is "no".
fn in_run_sandbox(pid: u32, marks: &RunMarks) -> bool {
    use super::sandbox_check::{is_sandboxed, may_write};
    is_sandboxed(pid) == Some(true)
        && may_write(pid, &marks.mark) == Some(true)
        && may_write(pid, &marks.control) == Some(false)
}

fn listed_pids(listing: &str) -> impl Iterator<Item = u32> + '_ {
    listing
        .lines()
        .filter_map(|line| line.split_whitespace().next()?.parse::<u32>().ok())
}

/// `self_pid`, its ancestors in `listing`, 0 and 1: never reaped.
fn self_and_ancestors(listing: &str, self_pid: u32) -> HashSet<u32> {
    let parent_of: HashMap<u32, u32> = listing
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse::<u32>().ok()?;
            let ppid = fields.next()?.parse::<u32>().ok()?;
            Some((pid, ppid))
        })
        .collect();
    let mut protected = HashSet::from([0, 1, self_pid]);
    let mut cursor = self_pid;
    while let Some(&parent) = parent_of.get(&cursor) {
        if !protected.insert(parent) {
            break;
        }
        cursor = parent;
    }
    protected
}

/// Pure part of [`reap`]: which pids of a `pid ppid command+environment`
/// listing belong to `run_id`. `self_pid` and its ancestors are never chosen.
pub fn select_victims(listing: &str, run_id: &str, self_pid: u32) -> Vec<u32> {
    let token = format!("{RUN_ID_ENV}={run_id}");
    let mut parent_of: HashMap<u32, u32> = HashMap::new();
    let mut matched: Vec<u32> = Vec::new();
    for line in listing.lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(ppid)) = (
            fields.next().and_then(|f| f.parse::<u32>().ok()),
            fields.next().and_then(|f| f.parse::<u32>().ok()),
        ) else {
            continue;
        };
        parent_of.insert(pid, ppid);
        // Whole-word comparison: run `abc` must not match `abcd`.
        if fields.any(|field| field == token) {
            matched.push(pid);
        }
    }

    let mut protected = HashSet::from([0, 1, self_pid]);
    let mut cursor = self_pid;
    while let Some(&parent) = parent_of.get(&cursor) {
        if !protected.insert(parent) {
            break;
        }
        cursor = parent;
    }

    let mut victims: HashSet<u32> = matched.into_iter().collect();
    extend_with_descendants(&mut victims, &parent_of);
    let mut victims: Vec<u32> = victims
        .into_iter()
        .filter(|pid| !protected.contains(pid))
        .collect();
    victims.sort_unstable();
    victims
}

/// Adds every pid whose ancestor chain reaches a member of `set`.
fn extend_with_descendants(set: &mut HashSet<u32>, parent_of: &HashMap<u32, u32>) {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (&pid, &ppid) in parent_of {
        children.entry(ppid).or_default().push(pid);
    }
    let mut queue: Vec<u32> = set.iter().copied().collect();
    while let Some(pid) = queue.pop() {
        for &child in children.get(&pid).map(Vec::as_slice).unwrap_or_default() {
            if set.insert(child) {
                queue.push(child);
            }
        }
    }
}

/// Remembers the descendants of one root process across samples.
///
/// A process is identified by pid plus start time, so a recycled pid is never
/// mistaken for a tracked one.
#[derive(Debug)]
pub struct ProcessTracker {
    root: u32,
    root_sampled: bool,
    seen: HashMap<u32, String>,
}

impl ProcessTracker {
    pub fn new(root_pid: u32) -> Self {
        ProcessTracker {
            root: root_pid,
            root_sampled: false,
            seen: HashMap::new(),
        }
    }

    /// Pids recorded so far.
    pub fn tracked(&self) -> Vec<u32> {
        let mut pids: Vec<u32> = self.seen.keys().copied().collect();
        pids.sort_unstable();
        pids
    }

    /// Takes one sample of the process table and records the root, every
    /// tracked process that is still the same process, and their descendants.
    pub fn observe(&mut self) -> PortResult<()> {
        let table = process_table(&system_ps, 1)?;
        let mut live: HashSet<u32> = self
            .seen
            .iter()
            .filter(|(pid, started)| table.get(pid).is_some_and(|row| &row.started == *started))
            .map(|(pid, _)| *pid)
            .collect();
        // Only the first sample may adopt the root by pid alone: afterwards
        // the root may have exited and its pid been reused.
        if !self.root_sampled && table.contains_key(&self.root) {
            live.insert(self.root);
        }
        self.root_sampled = true;
        let parent_of: HashMap<u32, u32> =
            table.iter().map(|(pid, row)| (*pid, row.ppid)).collect();
        extend_with_descendants(&mut live, &parent_of);
        for pid in live {
            if let Some(row) = table.get(&pid) {
                self.seen.insert(pid, row.started.clone());
            }
        }
        Ok(())
    }

    /// Kills every tracked process that is still alive. Returns their pids.
    pub fn kill_survivors(&self) -> PortResult<Vec<u32>> {
        let table = process_table(&system_ps, PS_READS)?;
        let own = std::process::id();
        let mut killed = Vec::new();
        for (&pid, started) in &self.seen {
            let same = table.get(&pid).is_some_and(|row| &row.started == started);
            if same && pid != own && kill_pid(pid, Signal::Kill)? {
                killed.push(pid);
            }
        }
        killed.sort_unstable();
        Ok(killed)
    }
}

struct ProcessRow {
    ppid: u32,
    started: String,
}

fn process_table(ps: Ps, tries: usize) -> PortResult<HashMap<u32, ProcessRow>> {
    let out = ps_listing(ps, &["-Ao", "pid=,ppid=,lstart="], tries)?;
    let mut table = HashMap::new();
    for line in out.lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(ppid)) = (
            fields.next().and_then(|f| f.parse::<u32>().ok()),
            fields.next().and_then(|f| f.parse::<u32>().ok()),
        ) else {
            continue;
        };
        let started = fields.collect::<Vec<_>>().join(" ");
        table.insert(pid, ProcessRow { ppid, started });
    }
    Ok(table)
}

#[cfg(test)]
mod tests;
