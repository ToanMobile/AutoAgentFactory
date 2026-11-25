//! RFC-033: a task blocked by upstream infra is scheduled to come back on
//! its own with backoff (60 s doubling to 30 min, at most 6 times), without
//! a webhook per attempt and without counting towards the global freeze.

use std::sync::atomic::Ordering;

use aaf_domain::blocked::BlockedReason;
use aaf_domain::event::EventKind;
use aaf_domain::ports::{EventSink, TaskStore};
use aaf_domain::task::TaskState;

use super::world::World;
use crate::factory::infra_retry::{INFRA_RETRY_MAX, block_infra, infra_backoff_secs};

fn infra() -> BlockedReason {
    BlockedReason::Infra {
        detail: "api 529 overloaded".into(),
    }
}

fn to_blocked(world: &World, id: &str) {
    for state in [TaskState::Running] {
        world.db.set_state(id, state, None, 900).unwrap();
    }
}

fn webhooks(world: &World) -> usize {
    world.notifier.0.lock().unwrap().len()
}

#[test]
fn backoff_doubles_from_a_minute_and_caps_at_thirty_minutes() {
    let got: Vec<i64> = (0..INFRA_RETRY_MAX + 2).map(infra_backoff_secs).collect();
    assert_eq!(got, [60, 120, 240, 480, 960, 1_800, 1_800, 1_800]);
}

#[test]
fn infra_block_schedules_a_silent_retry_that_the_scheduler_wakes() {
    let world = World::new();
    world.add("t1");
    to_blocked(&world, "t1");
    let deps = world.deps();
    block_infra(&deps, &world.task("t1"), &infra(), "unblock T5: infra").unwrap();

    let task = world.task("t1");
    assert_eq!(task.state, TaskState::Blocked);
    assert_eq!(task.retry_after, Some(1_000 + 60));
    assert_eq!(webhooks(&world), 0, "intermediate attempts send no webhook");
    let ev = world
        .db
        .events_for_task_by_kinds("t1", &[EventKind::TaskStateChanged], 1)
        .unwrap();
    let payload: serde_json::Value = serde_json::from_str(&ev[0].payload_json).unwrap();
    assert_eq!(payload["to"], "blocked");
    assert_eq!(payload["auto_retry_at"], 1_060);

    assert!(
        world
            .db
            .wake_due_blocked_tasks(&deps.settings.repo_root, 1_059)
            .unwrap()
            .is_empty()
    );
    let woken = world
        .db
        .wake_due_blocked_tasks(&deps.settings.repo_root, 1_060)
        .unwrap();
    assert_eq!(woken, ["t1"]);
    assert_eq!(world.state("t1"), TaskState::Ready);
    assert_eq!(world.task("t1").auto_recover_count, 1);
}

#[test]
fn after_the_last_retry_the_block_is_final_and_notifies_once() {
    let world = World::new();
    world.add("t1");
    let deps = world.deps();
    let mut now = 1_000;
    for attempt in 0..INFRA_RETRY_MAX {
        to_blocked(&world, "t1");
        world.clock.0.store(now, Ordering::SeqCst);
        block_infra(&deps, &world.task("t1"), &infra(), "infra").unwrap();
        let due = now + infra_backoff_secs(attempt);
        assert_eq!(world.task("t1").retry_after, Some(due));
        now = due;
        assert_eq!(
            world
                .db
                .wake_due_blocked_tasks(&deps.settings.repo_root, now)
                .unwrap()
                .len(),
            1
        );
    }
    assert_eq!(webhooks(&world), 0);
    to_blocked(&world, "t1");
    block_infra(&deps, &world.task("t1"), &infra(), "infra").unwrap();
    let task = world.task("t1");
    assert_eq!(task.state, TaskState::Blocked);
    assert_eq!(task.retry_after, None);
    assert_eq!(webhooks(&world), 1, "the final block tells a person");
    assert!(
        world
            .db
            .wake_due_blocked_tasks(&deps.settings.repo_root, now + 99_999)
            .unwrap()
            .is_empty()
    );
}
