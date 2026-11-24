//! Throttle tests. Waits and measurements run on fakes; the files named
//! `real_*` start small real processes (`sleep`, `sh -c`) to prove the parts
//! that only the operating system can prove.

mod admit;
mod daemons;
mod drain;
#[path = "../../../../aaf-fp/tests/exec_support.rs"]
pub(crate) mod exec_support;
mod fifo_queue;
mod guest_window;
mod heavy;
mod layer1_budget;
mod legacy;
mod orphan_slots;
mod orphans;
mod private;
mod probe;
mod real_run;
mod real_slots;
mod relay;
mod reservations;
mod slot_run;
mod slots;
mod stats;
pub(crate) mod support;
mod token;
mod tree;
mod watchdog;
