//! The adopt lane end to end on real git: a fake `gh`, two scripted reviewers,
//! an `Adopting` task made by direct insert plus its `PrAdopted` event.

mod aborts;
mod e2e;
mod retries;
mod round;
mod sched;
mod support;
