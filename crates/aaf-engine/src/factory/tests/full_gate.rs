//! The idle full gate on the base branch (`[full_gate]`).

use std::sync::atomic::Ordering;

use aaf_domain::config::{FullGateConfig, LandMode};
use aaf_domain::ports::TaskStore;
use aaf_domain::task::{Priority, TaskState};

use super::world::World;
use crate::factory::full_gate::{Outcome, land_for, tick};

const HOUR: i64 = 3600;

fn world() -> World {
    let mut w = World::new();
    w.auto_merge = true;
    w.repo.run.auto_merge = true;
    w.repo.run.land = LandMode::Auto;
    w.repo.full_gate = FullGateConfig {
        cooldown_hours: 24,
        max_days: 7,
    };
    w
}

fn later(w: &World, secs: i64) {
    w.clock.0.fetch_add(secs, Ordering::SeqCst);
}

fn new_base(w: &World) {
    w.git.0.lock().unwrap().base += 1;
}

fn full_runs(w: &World) -> u32 {
    w.gate.state.lock().unwrap().full_runs
}

#[test]
fn it_is_off_without_a_cooldown() {
    let w = World::new();
    assert_eq!(tick(&w.deps()).unwrap(), Outcome::Skipped("off"));
    assert_eq!(full_runs(&w), 0);
}

#[test]
fn an_idle_factory_checks_each_new_base_once_per_cooldown() {
    let w = world();
    let deps = w.deps();
    assert_eq!(
        tick(&deps).unwrap(),
        Outcome::Green {
            sha: "base0".into()
        }
    );
    new_base(&w);
    assert_eq!(tick(&deps).unwrap(), Outcome::Skipped("cooldown"));
    later(&w, 24 * HOUR);
    assert_eq!(
        tick(&deps).unwrap(),
        Outcome::Green {
            sha: "base1".into()
        }
    );
    later(&w, 24 * HOUR);
    assert_eq!(tick(&deps).unwrap(), Outcome::Skipped("unchanged"));
    assert_eq!(full_runs(&w), 2);
}

#[test]
fn a_busy_factory_waits_until_max_days() {
    let w = world();
    let deps = w.deps();
    w.add("busy");
    // The seven days count from the first time the factory looked.
    assert_eq!(tick(&deps).unwrap(), Outcome::Skipped("busy"));
    later(&w, 6 * 24 * HOUR);
    assert_eq!(tick(&deps).unwrap(), Outcome::Skipped("busy"));
    later(&w, 24 * HOUR);
    assert!(matches!(tick(&deps).unwrap(), Outcome::Green { .. }));
}

#[test]
fn a_red_base_opens_one_p0_task_and_holds_auto_merge_until_green() {
    let w = world();
    let deps = w.deps();
    let other = w.add("other");
    w.db.set_state("other", TaskState::Blocked, None, 1)
        .unwrap();
    w.gate.state.lock().unwrap().full_red = true;

    let Outcome::Red { task_id, .. } = tick(&deps).unwrap() else {
        panic!("expected red");
    };
    let fix = w.task(&task_id);
    assert_eq!((fix.priority, fix.state), (Priority::P0, TaskState::Ready));
    assert!(fix.title.starts_with("Sửa main"), "{}", fix.title);
    assert_eq!(land_for(&deps, &other), LandMode::Pr);
    assert_eq!(land_for(&deps, &fix), LandMode::Auto);

    // The fix task leaves the queue; the base is checked again at once
    // when it moves, with no cooldown.
    w.db.set_state(&task_id, TaskState::Blocked, None, 2)
        .unwrap();
    assert_eq!(tick(&deps).unwrap(), Outcome::Skipped("still red"));
    new_base(&w);
    w.gate.state.lock().unwrap().full_red = false;
    assert!(matches!(tick(&deps).unwrap(), Outcome::Green { .. }));
    assert_eq!(land_for(&deps, &other), LandMode::Auto);
}

#[test]
fn a_second_red_run_adds_to_the_open_fix_task() {
    let w = world();
    let deps = w.deps();
    w.gate.state.lock().unwrap().full_red = true;
    let Outcome::Red { task_id: first, .. } = tick(&deps).unwrap() else {
        panic!("expected red");
    };
    // Still open, and Ready: park it so the factory is idle again.
    w.db.set_state(&first, TaskState::Blocked, None, 2).unwrap();
    new_base(&w);
    let Outcome::Red {
        task_id: second, ..
    } = tick(&deps).unwrap()
    else {
        panic!("expected red");
    };
    assert_eq!(first, second);
    assert!(!w.task(&first).context_notes.is_empty());
}

#[test]
fn a_test_that_passes_on_rerun_is_flaky_and_holds_nothing() {
    let w = world();
    let deps = w.deps();
    {
        let mut gate = w.gate.state.lock().unwrap();
        gate.full_red = true;
        gate.full_flaky = true;
    }
    let Outcome::Flaky { task_id, .. } = tick(&deps).unwrap() else {
        panic!("expected flaky");
    };
    assert_eq!(w.task(&task_id).priority, Priority::P2);
    let probe = w.add("probe");
    assert_eq!(land_for(&deps, &probe), LandMode::Auto);
}
