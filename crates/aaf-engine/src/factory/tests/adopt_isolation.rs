//! `TaskState::Adopting` is isolated from the generic scheduler: no mode of
//! `run`, no sweep and no merge retry selects an adopted PR, and `recover`
//! reaps its zombie runs without moving it. The bait on the task (elapsed
//! `retry_after`, PR events with a retryable `ci_pending`, a branch the fake git
//! reports as merged, a scripted merge) is what a state-blind path would act on.

use std::sync::Mutex;

use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{EventSink, RunStore, TaskStore};
use aaf_domain::run::{RunRecord, RunStatus};
use aaf_domain::task::{Priority, Task, TaskState};
use serde_json::json;

use super::fakes::{REPO, spec};
use super::world::World;
use crate::factory::Mode;
use crate::factory::merge_retry::retry_task;
use crate::factory::sweep::recover;
use crate::pr_ops::PrMergeOutcome;

const URL: &str = "https://github.com/acme/app/pull/9";

fn run_record(id: &str, task_id: &str) -> RunRecord {
    RunRecord {
        id: id.to_string(),
        task_id: task_id.to_string(),
        account_id: None,
        executor: "fake".into(),
        worktree_path: format!("/fake/wt/{task_id}"),
        branch: format!("aaf/{task_id}"),
        status: RunStatus::Running,
        exit_code: None,
        tokens_in: 0,
        tokens_out: 0,
        started_at: 50,
        ended_at: None,
    }
}

/// An `Adopting` task with PR bait, inserted directly (nothing moves into it).
fn adopting(world: &World, id: &str) -> Task {
    let mut task = Task::new_root(id, REPO, format!("adopted {id}"), spec(), Priority::P0, 1);
    task.state = TaskState::Adopting;
    task.retry_after = Some(5);
    world.db.insert_task(&task).unwrap();
    for (kind, payload) in [
        (EventKind::PrAdopted, json!({ "pr_number": 9, "url": URL })),
        (EventKind::PrOpened, json!({ "url": URL })),
        (
            EventKind::MergeResult,
            json!({"merged": false, "kind": "pr_merge", "code": "ci_pending",
                   "url": URL, "head_sha": "abc123"}),
        ),
    ] {
        let ev = FactoryEvent::new(kind, payload.to_string(), 100).for_task(id);
        world.db.emit(&ev).unwrap();
    }
    {
        let mut git = world.git.0.lock().unwrap();
        git.merged.push(id.to_string());
        git.merge_pr_script
            .push_back(PrMergeOutcome::AlreadyMerged {
                pr_number: 9,
                url: URL.into(),
            });
    }
    world.db.get_task(id).unwrap()
}

fn event_count(world: &World, id: &str) -> usize {
    world.db.events_for_task(id, 1_000).unwrap().len()
}

#[test]
fn no_scheduler_mode_selects_an_adopting_task() {
    for mode in [Mode::Once, Mode::Drain, Mode::Auto] {
        let world = World::new();
        let before = adopting(&world, "t-ad");
        let events = event_count(&world, "t-ad");
        // A Ready neighbour proves the loop really ran and started work.
        world.add("t-ready");

        let summary = world.run(mode, &|| false);

        assert_eq!(summary.agent_runs, 1, "{mode:?}: only the Ready task ran");
        assert_ne!(world.state("t-ready"), TaskState::Ready, "{mode:?}");
        let after = world.task("t-ad");
        assert_eq!(after.state, TaskState::Adopting, "{mode:?}: {summary:?}");
        assert_eq!(after.retry_after, before.retry_after, "{mode:?}");
        assert_eq!(after.updated_at, before.updated_at, "{mode:?}");
        assert_eq!(event_count(&world, "t-ad"), events, "{mode:?}");
        assert!(
            world.db.runs_for_task("t-ad").unwrap().is_empty(),
            "{mode:?}"
        );
        for list in [
            &summary.done,
            &summary.review,
            &summary.blocked,
            &summary.requeued,
        ] {
            assert!(!list.iter().any(|id| id == "t-ad"), "{mode:?}: {summary:?}");
        }
        let git = world.git.0.lock().unwrap();
        assert!(
            git.merge_pr_calls.is_empty(),
            "{mode:?}: merge_pr was called"
        );
        assert!(!git.cleaned.iter().any(|id| id == "t-ad"), "{mode:?}");
        assert!(!git.log.iter().any(|l| l.ends_with("t-ad")), "{mode:?}");
        drop(git);
        assert!(
            !world
                .gate
                .state
                .lock()
                .unwrap()
                .runs
                .iter()
                .flatten()
                .any(|id| id == "t-ad"),
            "{mode:?}: gate ran for the adopting task"
        );
    }
}

#[test]
fn an_adopting_task_alone_is_idle_in_every_mode() {
    for mode in [Mode::Once, Mode::Drain, Mode::Auto] {
        let world = World::new();
        adopting(&world, "t-ad");
        let summary = world.run(mode, &|| false);
        assert_eq!(summary.agent_runs, 0, "{mode:?}");
        assert_eq!(summary.gate_runs, 0, "{mode:?}");
        assert_eq!(world.state("t-ad"), TaskState::Adopting, "{mode:?}");
    }
}

#[test]
fn merge_retry_never_merges_an_adopting_task() {
    let world = World::new();
    let task = adopting(&world, "t-ad");
    // 900 s after the `ci_pending` block: a Review task would be retried now.
    world
        .clock
        .0
        .store(1_000, std::sync::atomic::Ordering::SeqCst);

    assert!(!retry_task(&world.deps(), &task).unwrap());
    assert!(world.git.0.lock().unwrap().merge_pr_calls.is_empty());
    assert_eq!(world.state("t-ad"), TaskState::Adopting);
}

#[test]
fn recover_reaps_the_zombie_run_of_an_adopting_task_and_keeps_its_state() {
    let world = World::new();
    adopting(&world, "t-ad");
    world.db.insert_run(&run_record("run-ad", "t-ad")).unwrap();
    // Controls: a Running task goes back to Ready, a Gating task is handed back
    // for its gate, each with its zombie run reaped.
    world.add("t-run");
    world
        .db
        .set_state("t-run", TaskState::Running, None, 40)
        .unwrap();
    world
        .db
        .insert_run(&run_record("run-run", "t-run"))
        .unwrap();
    world.add("t-gate");
    for state in [TaskState::Running, TaskState::Gating] {
        world.db.set_state("t-gate", state, None, 40).unwrap();
    }
    world
        .db
        .insert_run(&run_record("run-gate", "t-gate"))
        .unwrap();
    let events = event_count(&world, "t-ad");

    let reaped = Mutex::new(Vec::new());
    let resumed = recover(&world.deps(), &|run_id| {
        reaped.lock().unwrap().push(run_id.to_string());
        Ok(())
    })
    .unwrap();

    let mut reaped = reaped.into_inner().unwrap();
    reaped.sort();
    assert_eq!(reaped, vec!["run-ad", "run-gate", "run-run"]);
    assert_eq!(
        resumed,
        vec!["t-gate".to_string()],
        "Adopting is not re-gated"
    );
    assert_eq!(world.state("t-run"), TaskState::Ready);
    assert_eq!(world.state("t-gate"), TaskState::Gating);
    // The adopted task stays Adopting: not Ready (the generic Running branch),
    // not Blocked, and nothing moved it.
    assert_eq!(world.state("t-ad"), TaskState::Adopting);
    assert_eq!(event_count(&world, "t-ad"), events);
    let runs = world.db.runs_for_task("t-ad").unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, RunStatus::Killed);
    assert!(runs[0].ended_at.is_some());
}
