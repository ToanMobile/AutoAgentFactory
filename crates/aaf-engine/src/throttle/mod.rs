//! Machine load control for heavy gate commands (docs/PLAN.md C.11): a
//! machine-wide slot, admission by learned memory use, a wait for calm
//! before the run, low priority, and a pause under critical memory pressure.
//! Load makes a command wait or run degraded; it never fails or kills it.

pub mod admit;
pub mod daemons;
mod drain;
pub mod heavy;
pub mod idle;
mod legacy;
pub mod orphans;
pub mod paths;
pub mod pools;
pub mod probe;
pub mod queue;
mod relay;
mod reservation;
mod run;
mod slot_file;
pub mod slots;
pub mod stats;
#[cfg(test)]
pub(crate) mod tests;
pub mod token;
pub mod watchdog;

use std::path::Path;
use std::time::Duration;

use aaf_domain::config::MachineConfig;

pub use admit::{Ask, DRAIN_AFTER_SECS};
pub use daemons::BuildDaemon;
pub use heavy::{
    HeavyTool, heavy_tool, heavy_tool_argv, stats_command_family, stats_command_family_argv,
    unqueued_heavy_tool,
};
pub use probe::{GroupState, LoadProbe, Pressure, SystemProbe};
pub use run::{SLOT_ENV, SlotJob, SlotRun};
pub use slots::{Admission, Busy, SlotGuard};
pub use stats::HeavySample;
pub use token::{SlotToken, inherited_slot};
pub use watchdog::{GroupControl, PriorityTools, SystemGroup, ThreadSleeper};

/// Pauses the calling thread. Injected so tests of the waits never sleep.
pub trait Sleeper: Send + Sync {
    fn sleep(&self, duration: Duration);
}

/// What the throttle is doing, for whoever shows it to the user (the CLI
/// prints these to stderr). Library code never prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress<'a> {
    WaitingForSlot {
        label: &'a str,
        reason: Busy,
        waited_secs: u64,
    },
    /// Memory pressure is not normal; this wait has no ceiling.
    WaitingForMemory {
        label: &'a str,
        pressure: Pressure,
        waited_secs: u64,
    },
    /// CPU load is above the limit; the command starts anyway once the load
    /// wait reaches `gives_up_after_secs`.
    WaitingForLoad {
        label: &'a str,
        load_percent: u32,
        max_load_percent: u32,
        waited_secs: u64,
        gives_up_after_secs: u64,
    },
    /// The running command was stopped under critical memory pressure.
    Paused {
        label: &'a str,
        slot: usize,
    },
    Resumed {
        label: &'a str,
        slot: usize,
    },
    /// A measurement could not be read and its fallback is in effect.
    ProbeUnreadable {
        what: &'static str,
    },
    /// The command's process group still had members after the kill was
    /// repeated for its whole bound; they may still be running.
    Unkilled {
        label: &'a str,
        pgid: u32,
    },
}

/// One heavy command to run.
#[derive(Debug, Clone)]
pub struct HeavyJob<'a> {
    /// Identifies the repository in `heavy_stats.json`.
    pub repo_id: &'a str,
    /// The command kind: `build`, `test_scoped`, `test_full`, or `lint`
    /// when the lint command starts a build tool.
    pub label: &'a str,
    /// Run through `/bin/sh -c` in `cwd`.
    pub command: &'a str,
    pub cwd: &'a Path,
    pub timeout: Duration,
    /// Trailing bytes of combined output to keep.
    pub log_tail_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeavyRun {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// Running time; time spent paused is left out.
    pub duration: Duration,
    pub output: Vec<u8>,
    /// Slot the command ended in.
    pub slot: usize,
    /// Seconds spent waiting for a slot and for the machine to calm down.
    pub waited_secs: u64,
    /// True when the CPU load never dropped and the command ran anyway.
    pub degraded: bool,
    pub paused: Duration,
    pub peak_rss_bytes: Option<u64>,
    /// Gradle or Kotlin daemons a Gradle command left running.
    pub leftover_daemons: Vec<BuildDaemon>,
}

/// A granted slot and how it was obtained.
#[derive(Debug)]
pub struct Admitted {
    pub slot: SlotGuard,
    pub waited_secs: u64,
    pub degraded: bool,
}

const BACKOFF_START: Duration = Duration::from_millis(250);
const BACKOFF_CAP: Duration = Duration::from_secs(8);
/// How often a running command's memory and the machine's pressure are read.
pub const SAMPLE_EVERY: Duration = Duration::from_secs(2);

/// Doubling delay with +/-25 % jitter, so waiters that started together do
/// not keep probing the locks at the same instant.
#[derive(Debug, Clone)]
pub struct Backoff {
    next: Duration,
    state: u64,
}

impl Backoff {
    pub fn new(seed: u64) -> Backoff {
        Backoff {
            next: BACKOFF_START,
            // xorshift has a fixed point at zero.
            state: seed | 1,
        }
    }

    pub fn next_delay(&mut self) -> Duration {
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        let base = self.next;
        self.next = (base * 2).min(BACKOFF_CAP);
        // 75 % to 125 % of the base, in steps of 1/512.
        let factor = 768 + u32::try_from(self.state % 513).unwrap_or(0);
        base * factor / 1024
    }
}

static THREAD_SLEEPER: ThreadSleeper = ThreadSleeper;
static SYSTEM_GROUP: SystemGroup = SystemGroup;

/// The load controller. Every dependency on the machine is a field, so tests
/// drive it with fakes. Admission is in [`admit`], the run itself in `run`.
pub struct Throttle<'a> {
    pub machine: &'a MachineConfig,
    pub probe: &'a dyn LoadProbe,
    pub sleeper: &'a dyn Sleeper,
    pub control: &'a dyn GroupControl,
    pub progress: &'a dyn Fn(&Progress<'_>),
    /// CPU count behind the automatic slot ceiling.
    pub cpus: usize,
    pub tools: PriorityTools,
    pub jitter_seed: u64,
    pub sample_every: Duration,
}

impl<'a> Throttle<'a> {
    /// The throttle of the real machine.
    pub fn system(
        machine: &'a MachineConfig,
        probe: &'a SystemProbe,
        progress: &'a dyn Fn(&Progress<'_>),
    ) -> Throttle<'a> {
        Throttle {
            machine,
            probe,
            sleeper: &THREAD_SLEEPER,
            control: &SYSTEM_GROUP,
            progress,
            cpus: probe.cpus,
            tools: PriorityTools::detect(),
            jitter_seed: u64::from(std::process::id()),
            sample_every: SAMPLE_EVERY,
        }
    }

    pub fn ceiling(&self) -> usize {
        slots::ceiling(self.machine.max_heavy_parallel, self.cpus)
    }
}
