//! What the first real `aaf run` (Wave 3 acceptance) got wrong, pinned on
//! the fakes: a pull request that cannot be opened, and a usage limit hit
//! while other accounts are free.

use aaf_domain::event::EventKind;
use aaf_domain::ports::{EventSink, KvStore, META_CONSECUTIVE_ROOT_FAILS, RunStore};
use aaf_domain::task::TaskState;

use super::fakes::limit_outcome;
use super::world::World;

const GH_ERROR: &str = "io: `gh pr create` failed (exit Some(1)): none of the git remotes \
                        configured for this repository point to a known GitHub host";

#[test]
fn a_clear_task_whose_pull_request_cannot_be_opened_still_lands_in_review() {
    let world = World::new();
    world.add("t1");
    world.git.0.lock().unwrap().pr_error = Some(GH_ERROR.to_string());

    let summary = world.once();

    // The gate was clear and the branch exists: the same outcome as a
    // repository without `gh`, never a blocked task or a failed root.
    assert_eq!(summary.review, vec!["t1"]);
    assert!(summary.blocked.is_empty(), "{:?}", summary.blocked);
    let task = world.task("t1");
    assert_eq!(task.state, TaskState::Review);
    assert_eq!(task.blocked_reason, None);
    assert!(world.notifier.0.lock().unwrap().is_empty());
    assert_eq!(
        world
            .db
            .get_meta(META_CONSECUTIVE_ROOT_FAILS)
            .unwrap()
            .as_deref(),
        Some("0"),
        "the root reached its merge point: the failure counter is reset, not raised"
    );

    // The reason is told twice: to whoever ran `aaf run`, and on the board.
    assert_eq!(summary.errors.len(), 1, "{:?}", summary.errors);
    assert!(summary.errors[0].contains("known GitHub host"));
    let events = world.db.events_for_task("t1", 100).unwrap();
    let moved = events
        .iter()
        .find(|e| e.kind == EventKind::TaskStateChanged && e.payload_json.contains("review"))
        .unwrap();
    assert!(
        moved.payload_json.contains("known GitHub host"),
        "{}",
        moved.payload_json
    );
    assert!(moved.payload_json.contains("aaf/t1"));
}

#[test]
fn a_limit_hit_restarts_the_task_on_a_free_account_in_the_same_run() {
    let world = World::with_accounts(&["a", "b"]);
    world.add("t1");
    world.runner.script("t1", vec![limit_outcome(None)]);

    let summary = world.once();

    assert_eq!(summary.review, vec!["t1"]);
    assert!(summary.requeued.is_empty(), "{:?}", summary.requeued);
    assert_eq!(summary.agent_runs, 2);
    let runs = world.db.runs_for_task("t1").unwrap();
    let accounts: Vec<Option<&str>> = runs.iter().map(|r| r.account_id.as_deref()).collect();
    assert_eq!(accounts, vec![Some("a"), Some("b")]);
}

#[test]
fn limits_on_every_account_end_the_run_with_the_task_waiting() {
    let world = World::with_accounts(&["a", "b"]);
    world.add("t1");
    world
        .runner
        .script("t1", vec![limit_outcome(None), limit_outcome(None)]);

    let summary = world.once();

    assert_eq!(summary.agent_runs, 2, "one run per account, no more");
    assert_eq!(summary.requeued, vec!["t1"]);
    assert_eq!(summary.waiting_for_account, 1);
    assert_eq!(world.state("t1"), TaskState::Ready);
}

#[test]
fn an_account_that_never_cools_down_cannot_make_one_run_loop() {
    // A reset time in the past (a wrong clock, a stale provider answer)
    // leaves the account free again at once.
    let world = World::new();
    world.add("t1");
    world.runner.script(
        "t1",
        (0..10).map(|_| limit_outcome(Some(1))).collect::<Vec<_>>(),
    );

    let summary = world.once();

    assert_eq!(
        summary.agent_runs, 2,
        "the first run and one restart for the single account"
    );
    assert_eq!(summary.requeued, vec!["t1"]);
    assert_eq!(world.state("t1"), TaskState::Ready);
}

#[test]
fn the_cooldown_event_keeps_what_the_provider_said() {
    let world = World::new();
    world.add("t1");
    world.runner.script("t1", vec![limit_outcome(Some(5_000))]);

    world.once();

    let events = world.db.events_for_task("t1", 100).unwrap();
    let cooldown = events
        .iter()
        .find(|e| e.kind == EventKind::AccountCooldown)
        .unwrap();
    assert!(
        cooldown.payload_json.contains("usage limit reached"),
        "{}",
        cooldown.payload_json
    );
}
