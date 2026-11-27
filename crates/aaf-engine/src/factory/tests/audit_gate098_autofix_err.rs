//! Regression verification for GATE-098: an `Err` from `GateRunner::run_autofix`
//! must prevent the task from entering the gate on a potentially dirty worktree.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use aaf_domain::event::EventKind;
use aaf_domain::gate::{HeadLockReport, Receipt};
use aaf_domain::ports::{EventSink, PortError, PortResult};
use aaf_domain::task::TaskState;

use super::fakes::FakeGate;
use super::world::World;
use crate::factory::ports::GateRunner;
use crate::factory::{Hooks, Mode, RunSummary, run};
use crate::gate::autofix::AutofixOutcome;
use crate::unblocker::RerunResult;

struct AutofixGate {
    inner: Arc<FakeGate>,
    autofix_error: Option<String>,
    autofix_calls: AtomicUsize,
}

impl AutofixGate {
    fn new(inner: Arc<FakeGate>, autofix_error: Option<&str>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            autofix_error: autofix_error.map(str::to_string),
            autofix_calls: AtomicUsize::new(0),
        })
    }

    fn autofix_calls(&self) -> usize {
        self.autofix_calls.load(Ordering::SeqCst)
    }
}

impl GateRunner for AutofixGate {
    fn run(&self, tree: &Path) -> PortResult<Receipt> {
        self.inner.run(tree)
    }

    fn run_full(&self, tree: &Path) -> PortResult<Receipt> {
        self.inner.run_full(tree)
    }

    fn head_lock(&self, tree: &Path) -> PortResult<HeadLockReport> {
        self.inner.head_lock(tree)
    }

    fn verify_cached_receipt(&self, tree: &Path, receipt: &Receipt) -> bool {
        self.inner.verify_cached_receipt(tree, receipt)
    }

    fn tree_binding(&self, tree: &Path) -> Option<(String, Option<String>)> {
        self.inner.tree_binding(tree)
    }

    fn rerun_failed_test(&self, tree: &Path, receipt: &Receipt) -> PortResult<Option<RerunResult>> {
        self.inner.rerun_failed_test(tree, receipt)
    }

    fn run_autofix(&self, tree: &Path) -> PortResult<AutofixOutcome> {
        self.autofix_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(msg) = &self.autofix_error {
            return Err(PortError::Io(msg.clone()));
        }
        self.inner.run_autofix(tree)
    }
}

fn run_once_with_gate(world: &World, gate: Arc<dyn GateRunner>) -> RunSummary {
    let mut deps = world.deps();
    deps.gate = gate;
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };
    run(&deps, Mode::Once, &hooks).unwrap()
}

#[test]
fn autofix_error_does_not_reach_gate() {
    let world = World::new();
    world.add("t1");
    let gate = AutofixGate::new(Arc::clone(&world.gate), Some("autofix cleanup failed"));

    let summary = run_once_with_gate(&world, Arc::clone(&gate) as Arc<dyn GateRunner>);

    assert_eq!(
        gate.autofix_calls(),
        1,
        "run_autofix must be invoked when the task has changes"
    );
    assert!(
        world.gate.runs().is_empty(),
        "GATE-098: gate must not be invoked after run_autofix returns Err, got runs={:?}",
        world.gate.runs()
    );
    assert_eq!(
        summary.gate_runs, 0,
        "GATE-098: summary must report 0 gate runs after run_autofix returns Err"
    );
    let state = world.state("t1");
    assert!(
        !matches!(state, TaskState::Done | TaskState::Review),
        "GATE-098: task must not reach Done/Review after autofix Err, got {state:?}"
    );
    let has_clear_receipt = world
        .db
        .events_for_task("t1", 100)
        .unwrap()
        .iter()
        .any(|ev| {
            ev.kind == EventKind::GateResult
                && serde_json::from_str::<serde_json::Value>(&ev.payload_json)
                    .ok()
                    .and_then(|v| v.get("status").and_then(|s| s.as_str()).map(str::to_string))
                    .is_some_and(|s| s.eq_ignore_ascii_case("clear"))
        });
    assert!(
        !has_clear_receipt,
        "GATE-098: task must not obtain a Clear gate receipt after autofix Err"
    );
}

#[test]
fn autofix_skipped_still_gates() {
    let world = World::new();
    world.add("t1");
    let gate = AutofixGate::new(Arc::clone(&world.gate), None);

    let summary = run_once_with_gate(&world, Arc::clone(&gate) as Arc<dyn GateRunner>);

    assert_eq!(gate.autofix_calls(), 1);
    assert_eq!(world.gate.runs(), vec![vec!["t1".to_string()]]);
    assert_eq!(summary.gate_runs, 1);
    assert_eq!(summary.review, vec!["t1"]);
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    assert_eq!(world.state("t1"), TaskState::Review);
}
