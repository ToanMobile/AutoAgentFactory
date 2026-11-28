//! The lane inside the scheduler: only `Mode::Auto` with an adopt environment
//! on the router picks an `Adopting` task, and a panic or a deferred reviewer
//! of that job never blocks a task, halts the factory or counts as a root
//! failure.

use std::sync::atomic::{AtomicUsize, Ordering};

use aaf_domain::event::EventKind;
use aaf_domain::ports::{KvStore, META_CONSECUTIVE_ROOT_FAILS};
use aaf_domain::task::TaskState;

use super::support::{ID, Lab, Pr};
use crate::factory::tests::fakes::limit_outcome;
use crate::factory::{Hooks, Mode, RunSummary, StageRouter, StageTarget, run_with_router_and_stop};

/// Runs the scheduler once over the lab's router. `Auto` is let to idle at
/// most `idles` times, as long as the task is still `Adopting`.
fn run(lab: &Lab, router: &StageRouter, mode: Mode, idles: usize) -> RunSummary {
    let db = lab.world.db.clone();
    let seen = AtomicUsize::new(0);
    let idle = move || {
        let adopting = aaf_domain::ports::TaskStore::get_task(&db, ID)
            .is_ok_and(|t| t.state == TaskState::Adopting);
        adopting && seen.fetch_add(1, Ordering::SeqCst) < idles
    };
    let hooks = Hooks {
        idle: &idle,
        reap: &|_| Ok(()),
    };
    run_with_router_and_stop(&lab.deps(), None, Some(router), mode, &hooks, &|| false).unwrap()
}

fn untouched(lab: &Lab) {
    assert_eq!(lab.state(), TaskState::Adopting);
    assert_eq!(lab.gh.views.load(Ordering::SeqCst), 0, "gh was not asked");
    assert_eq!(lab.gh.merges(), 0);
    assert!(lab.events(EventKind::AdoptStep).is_empty());
}

#[test]
fn once_and_drain_never_pick_an_adopting_task_even_with_the_lane_wired() {
    for mode in [Mode::Once, Mode::Drain] {
        let lab = Lab::new(Pr::Lib);
        let summary = run(&lab, &lab.router, mode, 0);
        untouched(&lab);
        assert!(
            summary.done.is_empty() && summary.blocked.is_empty(),
            "{mode:?}"
        );
    }
}

#[test]
fn auto_without_an_adopt_environment_leaves_the_task_alone() {
    let lab = Lab::new(Pr::Lib);
    let bare = StageRouter::new(StageTarget::new(
        "codex",
        "",
        "",
        "",
        lab.world.runner.clone(),
        lab.world.pool.clone(),
    ));
    run(&lab, &bare, Mode::Auto, 0);
    untouched(&lab);
}

#[test]
fn auto_takes_an_adopting_task_all_the_way_to_done() {
    let lab = Lab::new(Pr::Lib);
    let summary = run(&lab, &lab.router, Mode::Auto, 50);
    assert_eq!(
        lab.state(),
        TaskState::Done,
        "{summary:?} {:?}",
        lab.abort_code()
    );
    assert_eq!(lab.gh.merges(), 1);
    assert_eq!(lab.reviewer_runs(), 2);
    assert!(
        summary.blocked.is_empty() && summary.halted.is_none(),
        "{summary:?}"
    );
}

#[test]
fn a_panic_of_the_job_schedules_a_retry_and_blocks_nothing() {
    let lab = Lab::new(Pr::Lib);
    *lab.gh.panic_on_view.lock().unwrap() = true;
    let summary = run(&lab, &lab.router, Mode::Auto, 0);

    // The task is still Adopting, scheduled again, with the attempt on record.
    assert_eq!(lab.state(), TaskState::Adopting);
    assert_eq!(lab.task().retry_after, Some(1_000 + 60));
    assert!(lab.task().blocked_reason.is_none());
    let retry = lab
        .events(EventKind::AdoptStep)
        .into_iter()
        .find_map(|e| {
            let v: serde_json::Value = serde_json::from_str(&e.payload_json).ok()?;
            v["retry"].as_bool().filter(|r| *r)?;
            Some((v["attempt"].as_u64()?, v["code"].as_str()?.to_string()))
        })
        .expect("the retry is recorded");
    assert_eq!(retry, (1, "lane_panic".to_string()));
    // Nothing the generic worker path does to a task happened.
    assert!(summary.blocked.is_empty(), "{summary:?}");
    assert!(summary.halted.is_none() && !summary.frozen);
    assert!(
        summary.errors.iter().any(|e| e.contains("panicked")),
        "{:?}",
        summary.errors
    );
    let fails = lab.world.db.get_meta(META_CONSECUTIVE_ROOT_FAILS).unwrap();
    assert_eq!(fails, None, "no root result was recorded");
    assert!(lab.events(EventKind::AdoptAborted).is_empty());
}

#[test]
fn a_panic_after_a_reviewer_began_a_run_reaps_it_so_board_cancel_works() {
    use aaf_domain::ports::RunStore;
    use aaf_domain::run::RunStatus;

    let lab = Lab::new(Pr::Lib);
    // The first reviewer's run is in the store as `running` when the lane panics.
    *lab.runners[0].before.lock().unwrap() = Some(Box::new(|| panic!("reviewer exploded")));
    let summary = run(&lab, &lab.router, Mode::Auto, 0);

    let runs = lab.world.db.runs_for_task(ID).unwrap();
    assert!(!runs.is_empty(), "the reviewer turn did begin a run");
    assert!(
        runs.iter().all(|r| r.status != RunStatus::Running),
        "{runs:?}"
    );
    assert!(runs.iter().all(|r| r.ended_at.is_some()), "{runs:?}");
    // Still adopting, retried later, nothing blocked.
    assert_eq!(lab.state(), TaskState::Adopting);
    assert_eq!(lab.task().retry_after, Some(1_000 + 60));
    assert!(summary.blocked.is_empty(), "{summary:?}");
    // `board cancel`'s store path is no longer refused by a running run.
    let outcome = lab
        .world
        .db
        .cancel_task_atomic(ID, false, None, 2_000)
        .unwrap();
    assert!(outcome.is_ok(), "{outcome:?}");
    assert_eq!(lab.state(), TaskState::Cancelled);
}

#[test]
fn a_deferred_reviewer_in_the_scheduler_is_a_retry_not_a_halt_or_a_block() {
    let lab = Lab::new(Pr::Lib);
    lab.runners[0].fake.0.lock().unwrap().audit_outcome = Some(limit_outcome(Some(9_999)));
    let summary = run(&lab, &lab.router, Mode::Auto, 0);
    assert_eq!(lab.state(), TaskState::Adopting);
    assert!(lab.task().retry_after.is_some(), "scheduled again");
    assert!(summary.blocked.is_empty(), "{summary:?}");
    assert!(summary.halted.is_none(), "{summary:?}");
    assert!(!summary.frozen);
    assert_eq!(
        lab.world.db.get_meta(META_CONSECUTIVE_ROOT_FAILS).unwrap(),
        None
    );
}

#[test]
fn a_task_scheduled_for_later_is_not_started_yet() {
    let lab = Lab::new(Pr::Lib);
    aaf_domain::ports::TaskStore::set_retry_schedule(
        &lab.world.db,
        ID,
        Some(1_000 + 600),
        0,
        1_000,
    )
    .unwrap();
    run(&lab, &lab.router, Mode::Auto, 0);
    untouched(&lab);
}
