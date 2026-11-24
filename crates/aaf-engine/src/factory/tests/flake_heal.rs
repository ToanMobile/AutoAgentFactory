//! A flaky gate heals itself: the agent's rerun that changes nothing goes
//! through a real gate run again, and a pass there sends the task to Review
//! (no reused red receipt short-cuts it into the unblock ladder). Real git
//! and the real gate adapter; the agent is a closure.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use aaf_domain::event::EventKind;
use aaf_domain::ports::{AgentOutcome, AgentRequest, AgentRunner, EventSink, PortResult};
use aaf_domain::task::TaskState;

use super::fakes::ok_outcome;
use super::git_real::Fixture;
use super::world::World;
use crate::factory::{Deps, Hooks, Mode, RealGit, resolve_base, run};

/// Writes a feature file on its first agent run, changes nothing afterwards.
struct WriteOnce(AtomicUsize);
impl AgentRunner for WriteOnce {
    fn run(&self, req: &AgentRequest) -> PortResult<AgentOutcome> {
        if req.prompt.contains("Split it into at") {
            return Ok(AgentOutcome {
                final_text: "[]".into(),
                ..ok_outcome()
            });
        }
        if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
            fs::write(Path::new(&req.cwd).join("feature.txt"), "x\n").unwrap();
        }
        Ok(ok_outcome())
    }
}

/// A gate command that fails on its first run only; the run counter lives
/// outside the worktree, so the tree the gate judges is the same each time.
fn flaky_world(fx: &Fixture) -> (World, Deps, PathBuf) {
    let counter = fx.tmp.path().join("gate-runs");
    let world = World::with_accounts(&[]);
    world.add("t1");
    let mut deps = world.deps();
    deps.git = Arc::new(RealGit::new(
        fx.repo.clone(),
        fx.tmp.path().join("worktrees"),
        fx.tmp.path().join("data/locks"),
        resolve_base(&fx.repo, "auto").unwrap(),
    ));
    let c = counter.display();
    deps.gate = Arc::new(fx.gate(&format!(
        "n=$(cat '{c}' 2>/dev/null || echo 0); echo $((n+1)) > '{c}'; [ \"$n\" -ge 1 ]"
    )));
    deps.runner = Arc::new(WriteOnce(AtomicUsize::new(0)));
    (world, deps, counter)
}

fn gate_command_runs(counter: &Path) -> u32 {
    fs::read_to_string(counter)
        .map(|s| s.trim().parse().unwrap())
        .unwrap_or(0)
}

fn gate_results(world: &World) -> usize {
    world
        .db
        .events_for_task("t1", 200)
        .unwrap()
        .iter()
        .filter(|e| e.kind == EventKind::GateResult)
        .count()
}

#[test]
fn a_flaky_gate_failure_heals_on_a_rerun_that_changes_nothing() {
    let fx = Fixture::new();
    let (world, deps, counter) = flaky_world(&fx);
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };

    // The pure test failure is rerun on the same tree (flaky-test rerun,
    // main G1: two reruns, then a real re-gate): the passes heal it
    // without another agent run. 4 = gate + 2 reruns + re-gate.
    let first = run(&deps, Mode::Once, &hooks).unwrap();
    assert_eq!((first.agent_runs, first.gate_runs), (1, 2), "{first:?}");
    assert_eq!(gate_command_runs(&counter), 4, "the gate command ran again");
    assert_eq!(first.review, vec!["t1"], "{first:?}");
    assert!(first.blocked.is_empty(), "{first:?}");
    assert_eq!(world.state("t1"), TaskState::Review);

    // Healed for good: nothing is run again.
    let second = run(&deps, Mode::Once, &hooks).unwrap();
    assert_eq!((second.agent_runs, second.gate_runs), (0, 0), "{second:?}");
    assert_eq!(gate_command_runs(&counter), 4);
}

#[test]
fn a_flaky_gate_failure_heals_in_auto_mode_with_one_gate_per_agent_run() {
    let fx = Fixture::new();
    let (world, deps, counter) = flaky_world(&fx);
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };

    let mut summaries = Vec::new();
    // Auto may back the retry off; each call picks up where the last stopped.
    for _ in 0..3 {
        if world.state("t1") == TaskState::Review {
            break;
        }
        summaries.push(run(&deps, Mode::Auto, &hooks).unwrap());
    }
    assert_eq!(world.state("t1"), TaskState::Review, "{summaries:?}");
    let agent_runs: u32 = summaries.iter().map(|s| s.agent_runs).sum();
    let gate_runs: u32 = summaries.iter().map(|s| s.gate_runs).sum();
    // One agent run; the failed tests rerun twice on that tree, then one
    // real re-gate (main G1): gate + 2 reruns + re-gate, and no more.
    assert_eq!((agent_runs, gate_runs), (1, 2), "{summaries:?}");
    assert_eq!(
        gate_command_runs(&counter),
        4,
        "no further gate of one tree"
    );
    assert_eq!(gate_results(&world), 2, "the gate and the re-gate");
    assert!(
        summaries.iter().all(|s| s.blocked.is_empty()),
        "{summaries:?}"
    );
}
