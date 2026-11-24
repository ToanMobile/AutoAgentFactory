//! A run that failed for the account or the network (an expired login, no
//! connection) is not an attempt at the task: the task goes back to Ready
//! untouched, and the run stops instead of failing every task it would
//! start next for the same reason.

use aaf_domain::ports::{
    AgentOutcome, InfraFailure, InfraKind, KvStore, META_CONSECUTIVE_ROOT_FAILS, RunStore,
};
use aaf_domain::task::TaskState;

use super::fakes::{RunKind, failed_outcome};
use super::world::World;

/// What the Claude Code runner reports for the real not-logged-in run.
fn not_logged_in() -> AgentOutcome {
    AgentOutcome {
        infra: Some(InfraFailure {
            kind: InfraKind::Auth,
            raw: "Not logged in · Please run /login".to_string(),
        }),
        tokens_in: 0,
        tokens_out: 0,
        ..failed_outcome("Not logged in · Please run /login")
    }
}

#[test]
fn an_auth_failure_puts_the_task_back_untried_and_stops_the_run() {
    let world = World::new();
    for id in ["t1", "t2", "t3"] {
        world.add(id);
    }
    world.runner.script("t1", vec![not_logged_in()]);

    let summary = world.once();

    // Nothing else was started on a login that does not work.
    assert_eq!(summary.agent_runs, 1, "{summary:?}");
    assert_eq!(world.runner.count(&RunKind::Agent), 1);
    let halted = summary.halted.clone().expect("the run says why it stopped");
    assert!(halted.contains("t1"), "{halted}");
    assert!(halted.contains("Not logged in"), "{halted}");
    assert!(halted.contains("login"), "{halted}");

    // Not a failure of the task: no ladder step, no note, no cycle, no
    // failed root, nobody notified, not Blocked.
    let task = world.task("t1");
    assert_eq!(task.state, TaskState::Ready);
    assert_eq!(task.blocked_reason, None);
    assert_eq!(task.unblock_cycles, 0);
    assert!(task.context_notes.is_empty(), "{:?}", task.context_notes);
    assert!(summary.blocked.is_empty());
    assert_eq!(summary.requeued, vec!["t1"]);
    assert!(world.notifier.0.lock().unwrap().is_empty());
    assert_eq!(
        world.db.get_meta(META_CONSECUTIVE_ROOT_FAILS).unwrap(),
        None
    );
    assert_eq!(world.runner.count(&RunKind::Proposer), 0);
    // The run itself is on record, as it happened.
    assert_eq!(world.db.runs_for_task("t1").unwrap().len(), 1);
    for id in ["t2", "t3"] {
        assert_eq!(world.state(id), TaskState::Ready, "{id}");
        assert!(world.db.runs_for_task(id).unwrap().is_empty(), "{id}");
    }

    // Once the login works again, the next run takes the tasks as new.
    let summary = world.once();
    assert_eq!(summary.halted, None);
    assert_eq!(summary.review, vec!["t1", "t2", "t3"]);
    assert_eq!(world.task("t1").unblock_cycles, 0);
}

#[test]
fn a_network_failure_stops_the_run_the_same_way() {
    let world = World::new();
    world.add("t1");
    world.add("t2");
    let offline = AgentOutcome {
        infra: Some(InfraFailure {
            kind: InfraKind::Network,
            raw: "Unable to connect to API (ENOTFOUND)".to_string(),
        }),
        ..failed_outcome("Unable to connect to API (ENOTFOUND)")
    };
    world.runner.script("t1", vec![offline]);

    let summary = world.once();

    assert_eq!(summary.agent_runs, 1);
    let halted = summary.halted.expect("the run says why it stopped");
    assert!(halted.contains("network"), "{halted}");
    assert_eq!(world.state("t1"), TaskState::Ready);
    assert!(world.task("t1").context_notes.is_empty());
}

/// The cross-audit's agent fails on its login: no verdict, so nothing is
/// held against the task. It stays in Gating for the next run's gate.
#[test]
fn an_auth_failure_of_the_auditor_leaves_the_task_gating_and_stops_the_run() {
    let world = World::new();
    world.add("t1");
    world
        .gate
        .state
        .lock()
        .unwrap()
        .locked
        .insert("t1".to_string(), "lockfp1".to_string());
    world.runner.0.lock().unwrap().audit_outcome = Some(not_logged_in());

    let summary = world.once();

    let halted = summary.halted.clone().expect("the run says why it stopped");
    assert!(halted.contains("Not logged in"), "{halted}");
    assert_eq!(world.runner.count(&RunKind::Audit), 1);
    let task = world.task("t1");
    assert_eq!(task.state, TaskState::Gating);
    assert!(task.context_notes.is_empty(), "{:?}", task.context_notes);
    assert_eq!(task.unblock_cycles, 0);
    assert!(summary.requeued.is_empty() && summary.blocked.is_empty());

    // With the login back, the audit runs again and passes.
    let summary = world.once();
    assert_eq!(summary.halted, None);
    assert_eq!(summary.review, vec!["t1"]);
    assert_eq!(world.runner.count(&RunKind::Audit), 2);
}

/// The neighbour: a failure of the task itself still climbs the ladder and
/// does not stop the run.
#[test]
fn an_ordinary_failure_still_counts_and_the_run_goes_on() {
    let world = World::new();
    world.add("t1");
    world.add("t2");
    world
        .runner
        .script("t1", vec![failed_outcome("compile error")]);

    let summary = world.once();

    assert_eq!(summary.halted, None);
    assert!(summary.agent_runs >= 2, "{summary:?}");
    assert_eq!(world.task("t1").unblock_cycles, 1);
    assert!(!world.task("t1").context_notes.is_empty());
}

/// O12-2: a 429 the auditor only printed (no structured reset time) cools
/// its account down for the default rate-limit pause, not the router's
/// 5-hour fallback for an unknown reset.
#[test]
fn a_heuristic_auditor_429_cools_the_account_for_the_default_pause() {
    let world = World::new();
    world.add("t1");
    world
        .gate
        .state
        .lock()
        .unwrap()
        .locked
        .insert("t1".to_string(), "lockfp1".to_string());
    world.runner.0.lock().unwrap().audit_outcome =
        Some(failed_outcome("API error: HTTP 429 Too Many Requests"));

    world.once();

    assert_eq!(world.runner.count(&RunKind::Audit), 1);
    let cooldown = world.pool.state.lock().unwrap().cooldown.get("a").copied();
    assert_eq!(
        cooldown,
        Some(1_000 + crate::factory::scheduler::DEFAULT_RATE_LIMIT_COOLDOWN_SECS)
    );
}
