//! An idle machine for the gate tests (included with `#[path]`; as a test
//! target of its own it holds no tests). With the real `SystemProbe` a heavy
//! gate command waits for as long as the machine running the tests is busy
//! or short of memory.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use aaf_domain::config::MachineConfig;
use aaf_engine::throttle::{
    LoadProbe, Pressure, PriorityTools, Progress, SAMPLE_EVERY, SystemGroup, ThreadSleeper,
    Throttle,
};

/// No load, no memory pressure, all the memory in the world.
pub struct Calm;

impl LoadProbe for Calm {
    fn load_percent(&self) -> Option<u32> {
        Some(0)
    }

    fn pressure(&self) -> Option<Pressure> {
        Some(Pressure::Normal)
    }

    fn free_memory_bytes(&self) -> Option<u64> {
        Some(u64::MAX)
    }
}

fn quiet(_: &Progress<'_>) {}

/// The real throttle (sleeper, process groups, priority tools) on a
/// [`Calm`] machine with four CPUs.
pub fn calm_throttle(machine: &MachineConfig) -> Throttle<'_> {
    Throttle {
        machine,
        probe: &Calm,
        sleeper: &ThreadSleeper,
        control: &SystemGroup,
        progress: &quiet,
        cpus: 4,
        tools: PriorityTools::detect(),
        jitter_seed: 1,
        sample_every: SAMPLE_EVERY,
    }
}
