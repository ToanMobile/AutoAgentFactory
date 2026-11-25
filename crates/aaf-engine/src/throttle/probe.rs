//! Machine load measurements. Every parser is a pure function of the tool's
//! output, so the decisions built on them are tested without the real machine.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// Memory pressure as the kernel reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pressure {
    Normal,
    Warn,
    Critical,
}

/// What the throttle needs to know about the machine. `None` means the value
/// could not be read; the caller applies the documented fallback.
pub trait LoadProbe: Send + Sync {
    /// 1-minute load average as a percentage of the CPU count.
    fn load_percent(&self) -> Option<u32>;
    /// Real system-wide CPU busy percentage (`0..=100`, i.e. `100 - idle%`),
    /// queried lazily only when `load_percent` exceeds `max_load_percent`.
    /// Default `None` falls back to `load_percent` alone.
    fn cpu_busy_percent(&self) -> Option<u32> {
        None
    }
    fn pressure(&self) -> Option<Pressure>;
    /// Memory a new process can take without swapping, in bytes.
    fn free_memory_bytes(&self) -> Option<u64>;
}

/// Budget for one measurement tool. They answer in milliseconds; a tool that
/// hangs must not hang the gate.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// Budget for a 1-second real-CPU sample (`iostat -C -n 0 -c 2 -w 1`).
const CPU_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// PSI `full avg10` at or above this is `Critical`.
/// ponytail: project default, not measured on a real Linux host; tune it
/// when the factory first runs on Linux.
const PSI_FULL_CRITICAL: f64 = 5.0;
/// PSI `some avg10` at or above this is `Warn`. Same caveat as above.
const PSI_SOME_WARN: f64 = 10.0;

fn number(token: &str) -> Option<f64> {
    // Some locales print a decimal comma.
    token
        .replace(',', ".")
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value >= 0.0)
}

/// 1-minute load average from `sysctl -n vm.loadavg` (`{ 1.50 2.00 3.00 }`)
/// or `/proc/loadavg` (`1.50 2.00 3.00 1/467 1234`).
pub fn parse_loadavg(text: &str) -> Option<f64> {
    let mut tokens = text.split_whitespace().peekable();
    if tokens.peek() == Some(&"{") {
        tokens.next();
    }
    number(tokens.next()?)
}

/// Load average as a rounded percentage of `cpus`.
pub fn load_percent(load: f64, cpus: usize) -> u32 {
    // Float-to-integer `as` saturates, so an absurd load cannot wrap.
    (load * 100.0 / cpus.max(1) as f64).round() as u32
}

/// Real CPU busy percentage (`0..=100`) from macOS `iostat -C -n 0 -c 2 -w 1`.
///
/// Requires the `us sy id` header and at least two data rows (row 1 is the
/// since-boot average; row 2 is the live 1-second sample). Returns `100 - id`.
pub fn parse_macos_iostat_cpu(text: &str) -> Option<u32> {
    let mut id_col: Option<usize> = None;
    let mut samples: Vec<u32> = Vec::new();
    for line in text.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.is_empty() {
            continue;
        }
        if id_col.is_none() {
            if let Some(pos) = cols.windows(3).position(|w| w == ["us", "sy", "id"]) {
                id_col = Some(pos + 2);
            }
            continue;
        }
        if let Some(col) = id_col
            && let Some(raw) = cols.get(col)
            && let Ok(idle) = raw.parse::<u32>()
            && idle <= 100
        {
            samples.push(100u32.saturating_sub(idle));
        }
    }
    (samples.len() >= 2).then(|| *samples.last().unwrap_or(&0))
}

/// Real CPU busy percentage (`0..=100`) from two snapshots of Linux `/proc/stat`.
///
/// Treats `idle` + `iowait` as non-CPU-busy so disk I/O wait does not inflate CPU busy.
pub fn parse_linux_proc_stat_cpu(before: &str, after: &str) -> Option<u32> {
    let parse_cpu = |text: &str| -> Option<(u64, u64)> {
        let line = text.lines().find(|l| l.starts_with("cpu "))?;
        let vals: Vec<u64> = line
            .split_whitespace()
            .skip(1)
            .filter_map(|t| t.parse().ok())
            .collect();
        if vals.len() < 4 {
            return None;
        }
        let idle = vals[3].saturating_add(vals.get(4).copied().unwrap_or(0));
        let total: u64 = vals.iter().copied().fold(0u64, u64::saturating_add);
        Some((total, idle))
    };
    let (t1, i1) = parse_cpu(before)?;
    let (t2, i2) = parse_cpu(after)?;
    let total_delta = t2.checked_sub(t1)?;
    if total_delta == 0 {
        return None;
    }
    let idle_delta = i2.saturating_sub(i1).min(total_delta);
    let busy_delta = total_delta - idle_delta;
    let pct = (busy_delta.saturating_mul(100) + total_delta / 2) / total_delta;
    Some(u32::try_from(pct.min(100)).unwrap_or(100))
}

/// `sysctl -n kern.memorystatus_vm_pressure_level`: 1 normal, 2 warn,
/// 4 critical. Anything else is unknown, never guessed.
pub fn parse_macos_pressure(text: &str) -> Option<Pressure> {
    match text.trim() {
        "1" => Some(Pressure::Normal),
        "2" => Some(Pressure::Warn),
        "4" => Some(Pressure::Critical),
        _ => None,
    }
}

/// Reclaimable memory from `vm_stat`: free, speculative, inactive and
/// purgeable pages times the page size.
pub fn parse_vm_stat(text: &str) -> Option<u64> {
    let (_, after) = text.lines().next()?.split_once("page size of ")?;
    let page_size: u64 = after.split_whitespace().next()?.parse().ok()?;
    let pages = |label: &str| -> Option<u64> {
        text.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            (name.trim() == label).then_some(())?;
            value.trim().trim_end_matches('.').parse().ok()
        })
    };
    // Free pages are the one figure nothing works without; the others only
    // add to it, and older systems do not print all of them.
    let mut total = pages("Pages free")?;
    for label in ["Pages speculative", "Pages inactive", "Pages purgeable"] {
        total = total.saturating_add(pages(label).unwrap_or(0));
    }
    Some(total.saturating_mul(page_size))
}

/// Linux `/proc/pressure/memory` (PSI).
pub fn parse_psi_memory(text: &str) -> Option<Pressure> {
    let avg10 = |kind: &str| -> Option<f64> {
        text.lines().find_map(|line| {
            let mut tokens = line.split_whitespace();
            (tokens.next()? == kind).then_some(())?;
            tokens.find_map(|token| number(token.strip_prefix("avg10=")?))
        })
    };
    let some = avg10("some")?;
    // Kernels before 5.13 print no `full` line for the root cgroup.
    let full = avg10("full").unwrap_or(0.0);
    Some(if full >= PSI_FULL_CRITICAL {
        Pressure::Critical
    } else if some >= PSI_SOME_WARN {
        Pressure::Warn
    } else {
        Pressure::Normal
    })
}

/// `MemAvailable` of `/proc/meminfo`, in bytes.
pub fn parse_meminfo(text: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let rest = line.strip_prefix("MemAvailable:")?;
        let mut tokens = rest.split_whitespace();
        let value: u64 = tokens.next()?.parse().ok()?;
        (tokens.next()? == "kB").then_some(())?;
        Some(value.saturating_mul(1024))
    })
}

/// Resident memory of every process in group `pgid`, in bytes, from
/// `ps -A -o pgid=,rss=` (KiB). `None` when the group has no process.
pub fn sum_group_rss(ps_output: &str, pgid: u32) -> Option<u64> {
    let mut total: Option<u64> = None;
    for line in ps_output.lines() {
        let mut tokens = line.split_whitespace();
        let (Some(group), Some(rss)) = (tokens.next(), tokens.next()) else {
            continue;
        };
        if group.parse::<u32>().ok() != Some(pgid) {
            continue;
        }
        if let Ok(kib) = rss.parse::<u64>() {
            total = Some(total.unwrap_or(0).saturating_add(kib.saturating_mul(1024)));
        }
    }
    total
}

/// One process as the tree listing shows it: a pid alone can be handed to
/// another process later, a pid with its start time cannot.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProcId {
    pub pid: u32,
    /// Start time as `ps` prints it.
    pub started: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcRow {
    pub id: ProcId,
    pub ppid: u32,
    pub pgid: u32,
    pub rss_kib: u64,
}

/// Columns of the listing [`parse_tree`] reads; `lstart` last, it has spaces.
pub const PS_TREE_COLUMNS: &str = "pid=,ppid=,pgid=,rss=,lstart=";

/// Rows of `ps -A -o pid=,ppid=,pgid=,rss=,lstart=`; a line that does not
/// parse is skipped.
pub fn parse_tree(ps_output: &str) -> Vec<ProcRow> {
    let mut rows = Vec::new();
    for line in ps_output.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        let [pid, ppid, pgid, rss, started @ ..] = words.as_slice() else {
            continue;
        };
        let (Ok(pid), Ok(ppid), Ok(pgid), Ok(rss_kib)) =
            (pid.parse(), ppid.parse(), pgid.parse(), rss.parse())
        else {
            continue;
        };
        rows.push(ProcRow {
            id: ProcId {
                pid,
                started: started.join(" "),
            },
            ppid,
            pgid,
            rss_kib,
        });
    }
    rows
}

/// The processes of a command whose process group is `pgid`: the group,
/// everything below one of its members whatever group that is in (Unity's
/// shader compilers and licensing client, a Gradle daemon the build
/// started), and every process in `known` (the members of earlier samples)
/// that is still the same process, with what is below it. The last part
/// keeps a helper that was reparented to pid 1 after its parent exited
/// (VBCSCompiler): seen once as a descendant, it stays attributed.
///
/// Not found: a helper that left the tree between two samples (it was never
/// seen below the command), and a daemon that was already running and is
/// only talked to.
pub fn tree_members(rows: &[ProcRow], pgid: u32, known: &[ProcId]) -> Vec<ProcId> {
    let mut inside: std::collections::HashSet<u32> = rows
        .iter()
        .filter(|row| row.pgid == pgid || known.contains(&row.id))
        .map(|row| row.id.pid)
        .collect();
    // pid 0 and 1 adopt everything that lost its parent: never a parent
    // that puts a process into the tree.
    loop {
        let before = inside.len();
        for row in rows {
            if row.ppid > 1 && inside.contains(&row.ppid) {
                inside.insert(row.id.pid);
            }
        }
        if inside.len() == before {
            break;
        }
    }
    rows.iter()
        .filter(|row| inside.contains(&row.id.pid))
        .map(|row| row.id.clone())
        .collect()
}

/// Resident memory of `members` in bytes; `None` when none of them runs.
pub fn tree_rss(rows: &[ProcRow], members: &[ProcId]) -> Option<u64> {
    let mut total: Option<u64> = None;
    for row in rows.iter().filter(|row| members.contains(&row.id)) {
        total = Some(
            total
                .unwrap_or(0)
                .saturating_add(row.rss_kib.saturating_mul(1024)),
        );
    }
    total
}

/// What `ps` says about a process group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupState {
    /// Start time of the process whose pid is the group id, as `ps` prints
    /// it; `None` when that process is gone and only others remain.
    pub leader_started: Option<String>,
    /// True when a process of the group is stopped.
    pub stopped: bool,
}

/// The process table gave no answer: `ps` is missing, failed, or did not
/// finish within [`PROBE_TIMEOUT`] (seen under load 50-75 while dozens of
/// other `ps` ran). Says nothing about any group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unreadable;

/// Columns of the listing [`group_state`] reads.
pub const PS_GROUP_COLUMNS: &str = "pid=,pgid=,state=,lstart=";

/// Group `pgid` in `ps -A -o pid=,pgid=,state=,lstart=`; `None` when it has
/// no process.
pub fn group_state(ps_output: &str, pgid: u32) -> Option<GroupState> {
    let mut found: Option<GroupState> = None;
    for line in ps_output.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        let [pid, group, state, started @ ..] = words.as_slice() else {
            continue;
        };
        if group.parse::<u32>().ok() != Some(pgid) {
            continue;
        }
        let entry = found.get_or_insert(GroupState {
            leader_started: None,
            stopped: false,
        });
        entry.stopped |= state.contains('T');
        if pid.parse::<u32>().ok() == Some(pgid) && !started.is_empty() {
            entry.leader_started = Some(started.join(" "));
        }
    }
    found
}

/// Columns of the listing [`parse_member_states`] reads; `lstart` last, it has spaces.
pub const PS_MEMBER_STATE_COLUMNS: &str = "pid=,stat=,lstart=";

/// Process identity and stopped status from `ps -A -o pid=,stat=,lstart=`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberState {
    pub id: ProcId,
    /// True when the `stat` column contains `'T'` (stopped).
    pub stopped: bool,
}

/// Parses rows of `ps -A -o pid=,stat=,lstart=`; malformed lines or lines
/// without a start time are skipped.
pub fn parse_member_states(ps_output: &str) -> Vec<MemberState> {
    let mut rows = Vec::new();
    for line in ps_output.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        let [pid, stat, started @ ..] = words.as_slice() else {
            continue;
        };
        if started.is_empty() {
            continue;
        }
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        rows.push(MemberState {
            id: ProcId {
                pid,
                started: started.join(" "),
            },
            stopped: stat.contains('T'),
        });
    }
    rows
}

/// PIDs from `members` that `ps_output` (`pid=,stat=,lstart=`) confirms
/// simultaneously satisfy: `pid > 1`, exact non-empty `lstart` match, and
/// `stat` contains `'T'` (stopped).
pub fn stopped_member_pids(ps_output: &str, members: &[ProcId]) -> Vec<u32> {
    let mut pids = Vec::new();
    for row in parse_member_states(ps_output) {
        if row.id.pid > 1
            && row.stopped
            && !row.id.started.is_empty()
            && members.contains(&row.id)
            && !pids.contains(&row.id.pid)
        {
            pids.push(row.id.pid);
        }
    }
    pids
}

pub(crate) const PS: &[&str] = &["/bin/ps", "/usr/bin/ps"];

/// First of `candidates` that exists. The tools are addressed by absolute
/// path so a `PATH` an agent edited cannot swap them.
pub(crate) fn first_existing(candidates: &[&'static str]) -> Option<&'static str> {
    candidates
        .iter()
        .copied()
        .find(|path| Path::new(path).exists())
}

/// Stdout of a measurement tool with an explicit timeout, or `None` on failure.
fn tool_output_with_timeout(
    candidates: &[&'static str],
    args: &[&str],
    timeout: Duration,
) -> Option<String> {
    let mut command = Command::new(first_existing(candidates)?);
    command.args(args).env("LC_ALL", "C");
    let finished = crate::process::run_with_timeout(command, timeout, true).ok()?;
    finished.status.filter(std::process::ExitStatus::success)?;
    String::from_utf8(finished.stdout).ok()
}

/// Stdout of a measurement tool, or `None` when it is missing, fails or
/// exceeds [`PROBE_TIMEOUT`].
pub(crate) fn tool_output(candidates: &[&'static str], args: &[&str]) -> Option<String> {
    tool_output_with_timeout(candidates, args, PROBE_TIMEOUT)
}

const SYSCTL: &[&str] = &["/usr/sbin/sysctl", "/sbin/sysctl"];
const VM_STAT: &[&str] = &["/usr/bin/vm_stat"];
const IOSTAT: &[&str] = &["/usr/sbin/iostat", "/usr/bin/iostat"];

/// Reads the real machine: `sysctl` and `vm_stat` on macOS, `/proc` elsewhere.
#[derive(Debug, Clone, Copy)]
pub struct SystemProbe {
    pub cpus: usize,
}

/// Logical CPUs available to this process (at least 1).
pub fn cpu_count() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
}

impl SystemProbe {
    pub fn detect() -> SystemProbe {
        SystemProbe { cpus: cpu_count() }
    }
}

impl LoadProbe for SystemProbe {
    fn load_percent(&self) -> Option<u32> {
        let text = if cfg!(target_os = "macos") {
            tool_output(SYSCTL, &["-n", "vm.loadavg"])?
        } else {
            fs::read_to_string("/proc/loadavg").ok()?
        };
        Some(load_percent(parse_loadavg(&text)?, self.cpus))
    }

    fn cpu_busy_percent(&self) -> Option<u32> {
        if cfg!(target_os = "macos") {
            let text = tool_output_with_timeout(
                IOSTAT,
                &["-C", "-n", "0", "-c", "2", "-w", "1"],
                CPU_PROBE_TIMEOUT,
            )?;
            parse_macos_iostat_cpu(&text)
        } else {
            let before = fs::read_to_string("/proc/stat").ok()?;
            std::thread::sleep(Duration::from_millis(250));
            let after = fs::read_to_string("/proc/stat").ok()?;
            parse_linux_proc_stat_cpu(&before, &after)
        }
    }

    fn pressure(&self) -> Option<Pressure> {
        if cfg!(target_os = "macos") {
            let level = tool_output(SYSCTL, &["-n", "kern.memorystatus_vm_pressure_level"])?;
            parse_macos_pressure(&level)
        } else {
            parse_psi_memory(&fs::read_to_string("/proc/pressure/memory").ok()?)
        }
    }

    fn free_memory_bytes(&self) -> Option<u64> {
        if cfg!(target_os = "macos") {
            parse_vm_stat(&tool_output(VM_STAT, &[])?)
        } else {
            parse_meminfo(&fs::read_to_string("/proc/meminfo").ok()?)
        }
    }
}
