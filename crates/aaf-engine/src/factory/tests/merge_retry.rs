//! Review tasks whose unattended merge was blocked by `ci_pending` or
//! `merge_in_progress` are retried by the review sweep with backoff and a cap.

use std::sync::atomic::Ordering;

use aaf_domain::config::LandMode;
use aaf_domain::event::EventKind;
use aaf_domain::ports::{EventSink, TaskStore};
use aaf_domain::task::TaskState;
use serde_json::json;

use super::world::World;
use crate::factory::merge_retry::{MERGE_RETRY_MAX, backoff_secs, retry_task};
use crate::git::PrOutcome;
use crate::pr_ops::PrMergeOutcome;

const URL: &str = "https://github.com/acme/app/pull/9";

fn set_clock(world: &World, ts: i64) {
    world.clock.0.store(ts, Ordering::SeqCst);
}

fn blocked(code: &str) -> PrMergeOutcome {
    PrMergeOutcome::Blocked {
        code: code.into(),
        reason: format!("{code} for test"),
        receipt: None,
    }
}

fn merged() -> PrMergeOutcome {
    PrMergeOutcome::AlreadyMerged {
        pr_number: 9,
        url: URL.into(),
    }
}

/// A task that reached Review with its PR open and the merge blocked by
/// `code` at `ts`, as `land = "pr-merge"` records it.
fn review_task_blocked(world: &World, id: &str, code: &str, ts: i64) {
    world.add(id);
    for state in [TaskState::Running, TaskState::Gating, TaskState::Review] {
        world.db.set_state(id, state, None, ts - 5).unwrap();
    }
    for (kind, payload) in [
        (EventKind::PrOpened, json!({ "url": URL })),
        (
            EventKind::MergeResult,
            json!({"merged": false, "kind": "pr_merge", "code": code, "url": URL, "head_sha": "abc123"}),
        ),
    ] {
        let ev = aaf_domain::event::FactoryEvent::new(kind, payload.to_string(), ts).for_task(id);
        world.db.emit(&ev).unwrap();
    }
}

fn merge_calls(world: &World) -> usize {
    world.git.0.lock().unwrap().merge_pr_calls.len()
}

fn script(world: &World, outcomes: Vec<PrMergeOutcome>) {
    world.git.0.lock().unwrap().merge_pr_script = outcomes.into();
}

#[test]
fn backoff_doubles_from_a_minute_and_caps_at_thirty_minutes() {
    assert_eq!(backoff_secs(1), 60);
    assert_eq!(backoff_secs(2), 120);
    assert_eq!(backoff_secs(3), 240);
    assert_eq!(backoff_secs(6), 1_800);
    assert_eq!(backoff_secs(20), 1_800);
}

#[test]
fn ci_pending_merge_is_retried_after_backoff_and_lands() {
    let world = World::new();
    review_task_blocked(&world, "t1", "ci_pending", 1_000);
    script(&world, vec![merged()]);
    let deps = world.deps();

    set_clock(&world, 1_030);
    assert!(!retry_task(&deps, &world.task("t1")).unwrap());
    assert_eq!(merge_calls(&world), 0, "backoff not elapsed");

    set_clock(&world, 1_061);
    assert!(retry_task(&deps, &world.task("t1")).unwrap());
    assert_eq!(world.state("t1"), TaskState::Done);
    let calls = world.git.0.lock().unwrap().merge_pr_calls.clone();
    assert_eq!(calls, vec![(URL.to_string(), "abc123".to_string())]);
}

#[test]
fn a_still_pending_retry_waits_longer_and_stays_in_review() {
    let world = World::new();
    review_task_blocked(&world, "t1", "merge_in_progress", 1_000);
    script(&world, vec![blocked("ci_pending"), merged()]);
    let deps = world.deps();

    set_clock(&world, 1_061);
    assert!(!retry_task(&deps, &world.task("t1")).unwrap());
    assert_eq!(world.state("t1"), TaskState::Review);
    // Second attempt waits backoff(2) from the last block.
    set_clock(&world, 1_061 + 119);
    assert!(!retry_task(&deps, &world.task("t1")).unwrap());
    assert_eq!(merge_calls(&world), 1);
    set_clock(&world, 1_061 + 120);
    assert!(retry_task(&deps, &world.task("t1")).unwrap());
    assert_eq!(world.state("t1"), TaskState::Done);
}

#[test]
fn retries_stop_at_the_cap_and_report_exhausted_once() {
    let world = World::new();
    review_task_blocked(&world, "t1", "ci_pending", 1_000);
    script(
        &world,
        (0..MERGE_RETRY_MAX + 3)
            .map(|_| blocked("ci_pending"))
            .collect(),
    );
    let deps = world.deps();
    let mut now = 1_000;
    for _ in 0..(MERGE_RETRY_MAX + 4) {
        now += 1_801;
        set_clock(&world, now);
        assert!(!retry_task(&deps, &world.task("t1")).unwrap());
    }
    // The land attempt counts as the first try.
    assert_eq!(merge_calls(&world), (MERGE_RETRY_MAX - 1) as usize);
    let exhausted = world
        .db
        .events_for_task_by_kinds("t1", &[EventKind::MergeResult], 100)
        .unwrap()
        .into_iter()
        .filter(|e| e.payload_json.contains("merge_retry_exhausted"))
        .count();
    assert_eq!(exhausted, 1);
    assert_eq!(world.state("t1"), TaskState::Review);
}

#[test]
fn failed_ci_or_a_review_block_is_never_retried() {
    for code in ["ci_failed", "cross_approval_required", "head_mismatch"] {
        let world = World::new();
        review_task_blocked(&world, "t1", code, 1_000);
        script(&world, vec![merged()]);
        set_clock(&world, 9_000);
        assert!(!retry_task(&world.deps(), &world.task("t1")).unwrap());
        assert_eq!(merge_calls(&world), 0, "{code}");
    }
}

#[test]
fn a_pr_merge_land_blocked_by_pending_ci_records_a_retryable_merge_result() {
    let mut world = World::new();
    world.repo.run.land = LandMode::PrMerge;
    world.add("t1");
    {
        let mut git = world.git.0.lock().unwrap();
        git.pr = Some(PrOutcome::Opened { url: URL.into() });
        git.merge_pr_script = vec![blocked("ci_pending")].into();
    }
    world.once();
    assert_eq!(world.state("t1"), TaskState::Review);
    let last = world
        .db
        .events_for_task_by_kinds("t1", &[EventKind::MergeResult], 10)
        .unwrap();
    let payload: serde_json::Value = serde_json::from_str(&last[0].payload_json).unwrap();
    assert_eq!(payload["code"], "ci_pending");
    assert_eq!(payload["merged"], false);
    assert_eq!(payload["url"], URL);
}

#[test]
fn the_review_sweep_of_a_factory_run_performs_the_retry() {
    let world = World::new();
    review_task_blocked(&world, "t1", "ci_pending", 1_000);
    script(&world, vec![merged()]);
    set_clock(&world, 5_000);
    let summary = world.once();
    assert_eq!(world.state("t1"), TaskState::Done, "{summary:?}");
    assert_eq!(merge_calls(&world), 1);
}
