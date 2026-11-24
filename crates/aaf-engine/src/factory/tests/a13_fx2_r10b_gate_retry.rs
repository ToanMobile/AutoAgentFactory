//! O12-R10-7: a gate run that failed for a reason of the moment (a git
//! timeout under host load, an oracle run that timed out) is run again
//! instead of blocking the task for a human; config errors and repeated
//! failures still block, with the last error.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use aaf_domain::gate::{GateStatus, HeadLockReport, Receipt};
use aaf_domain::ports::{PortError, PortResult};
use aaf_domain::task::TaskState;

use super::fakes::receipt;
use super::world::World;
use crate::factory::ports::GateRunner;
use crate::factory::{Hooks, Mode, RunSummary, run};

/// What one scripted gate run answers.
type Step = PortResult<(GateStatus, Vec<String>)>;

/// Answers each gate run from `script`, then `Clear` once it is empty.
struct Scripted {
    inner: Arc<super::fakes::FakeGate>,
    script: Mutex<VecDeque<Step>>,
}

impl GateRunner for Scripted {
    fn run(&self, tree: &Path) -> PortResult<Receipt> {
        let next = self.script.lock().unwrap().pop_front();
        match next {
            None => self.inner.run(tree),
            Some(Err(err)) => Err(err),
            Some(Ok((status, reasons))) => Ok(receipt(tree, status, reasons)),
        }
    }

    fn head_lock(&self, tree: &Path) -> PortResult<HeadLockReport> {
        self.inner.head_lock(tree)
    }
}

fn run_with(world: &World, script: Vec<Step>) -> RunSummary {
    let mut deps = world.deps();
    deps.gate = Arc::new(Scripted {
        inner: world.gate.clone(),
        script: Mutex::new(script.into()),
    });
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };
    run(&deps, Mode::Once, &hooks).unwrap()
}

fn git_timeout() -> Step {
    Err(PortError::Io(
        "git git diff --name-status timed out after 30s".to_string(),
    ))
}

fn oracle(reason: &str) -> Step {
    Ok((GateStatus::Unverified, vec![reason.to_string()]))
}

#[test]
fn a_gate_error_once_is_retried_and_the_task_lands_in_review() {
    let world = World::new();
    world.add("t1");
    let summary = run_with(&world, vec![git_timeout()]);
    assert_eq!(summary.review, vec!["t1"], "{summary:?}");
    assert_eq!(summary.gate_runs, 2);
    assert_eq!(world.state("t1"), TaskState::Review);
}

#[test]
fn an_oracle_timeout_once_is_retried_and_then_proven_clear() {
    let world = World::new();
    world.add("t1");
    let summary = run_with(&world, vec![oracle("oracle: inconclusive (timed out)")]);
    assert_eq!(summary.review, vec!["t1"], "{summary:?}");
    assert_eq!(summary.gate_runs, 2);
}

#[test]
fn a_config_error_still_blocks_at_once() {
    let world = World::new();
    world.add("t1");
    let err = Err(PortError::Io("config: unknown key gate.x".to_string()));
    let summary = run_with(&world, vec![err]);
    assert_eq!(summary.blocked, vec!["t1"], "{summary:?}");
    assert_eq!(summary.gate_runs, 1);
}

#[test]
fn an_unprepared_oracle_tree_is_not_retried() {
    let world = World::new();
    world.add("t1");
    let reason = "oracle: inconclusive (could not prepare reverted tree: io)";
    let summary = run_with(&world, vec![oracle(reason)]);
    assert_eq!(summary.blocked, vec!["t1"], "{summary:?}");
    assert_eq!(summary.gate_runs, 1);
}

#[test]
fn a_gate_that_keeps_erroring_blocks_with_the_last_error() {
    let world = World::new();
    world.add("t1");
    let summary = run_with(&world, (0..6).map(|_| git_timeout()).collect());
    assert_eq!(summary.blocked, vec!["t1"], "{summary:?}");
    assert_eq!(summary.gate_runs, 4, "three retries, then blocked");
    let task = world.task("t1");
    let why = format!("{:?}", task.blocked_reason);
    assert!(why.contains("timed out after 30s"), "{why}");
}
