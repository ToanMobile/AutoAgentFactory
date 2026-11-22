#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The task state machine and task construction rules.

use aaf_domain::DomainError;
use aaf_domain::budget::MAX_SUBTASK_DEPTH;
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};

use TaskState::{Backlog, Blocked, Cancelled, Done, Gating, Ready, Review, Running};

/// The table documented on `TaskState::can_transition_to`, written out again
/// on purpose: a change to the lifecycle must be made in two places.
const ALLOWED: [(TaskState, &[TaskState]); 8] = [
    (Backlog, &[Ready, Cancelled]),
    (Ready, &[Running, Blocked, Cancelled]),
    (Running, &[Gating, Blocked, Ready, Cancelled]),
    (Gating, &[Review, Done, Blocked, Ready, Cancelled]),
    (Review, &[Gating, Done, Blocked, Ready, Cancelled]),
    (Blocked, &[Ready, Cancelled]),
    (Done, &[]),
    (Cancelled, &[]),
];

fn spec() -> ImmutableSpec {
    ImmutableSpec {
        description: "do the thing".to_string(),
        acceptance: vec!["it is done".to_string()],
    }
}

#[test]
fn every_pair_matches_the_documented_table() {
    assert_eq!(ALLOWED.len(), TaskState::ALL.len());
    let mut allowed_count = 0;
    for (from, targets) in ALLOWED {
        for to in TaskState::ALL {
            let want = targets.contains(&to);
            assert_eq!(from.can_transition_to(to), want, "{from:?} -> {to:?}");
            allowed_count += usize::from(want);
        }
    }
    assert_eq!(allowed_count, 21);
}

#[test]
fn no_state_transitions_to_itself_or_back_to_backlog() {
    for state in TaskState::ALL {
        assert!(!state.can_transition_to(state), "{state:?}");
        assert!(!state.can_transition_to(Backlog), "{state:?}");
    }
}

#[test]
fn terminal_states_are_done_and_cancelled_and_have_no_exit() {
    for state in TaskState::ALL {
        let terminal = matches!(state, Done | Cancelled);
        assert_eq!(state.is_terminal(), terminal, "{state:?}");
        let has_exit = TaskState::ALL
            .into_iter()
            .any(|to| state.can_transition_to(to));
        assert_eq!(has_exit, !terminal, "{state:?}");
    }
}

#[test]
fn every_non_terminal_state_can_be_cancelled() {
    for state in TaskState::ALL {
        assert_eq!(
            state.can_transition_to(Cancelled),
            !state.is_terminal(),
            "{state:?}"
        );
    }
}

#[test]
fn blocked_is_only_left_through_ready_or_cancelled() {
    let exits: Vec<TaskState> = TaskState::ALL
        .into_iter()
        .filter(|to| Blocked.can_transition_to(*to))
        .collect();
    assert_eq!(exits, vec![Ready, Cancelled]);
}

#[test]
fn done_is_only_reached_after_the_gate() {
    let sources: Vec<TaskState> = TaskState::ALL
        .into_iter()
        .filter(|from| from.can_transition_to(Done))
        .collect();
    assert_eq!(sources, vec![Gating, Review]);
}

#[test]
fn transition_agrees_with_can_transition_to_and_names_both_states() {
    for from in TaskState::ALL {
        for to in TaskState::ALL {
            let got = from.transition(to);
            if from.can_transition_to(to) {
                assert_eq!(got, Ok(to));
            } else {
                assert_eq!(
                    got,
                    Err(DomainError::InvalidTransition {
                        from: from.as_str(),
                        to: to.as_str(),
                    })
                );
            }
        }
    }
    let err = Done.transition(Running).unwrap_err();
    assert_eq!(err.to_string(), "invalid task transition done -> running");
}

#[test]
fn new_root_is_its_own_root_at_depth_zero_in_backlog() {
    let task = Task::new_root("t1", "/repo", "Title", spec(), Priority::P1, 100);
    assert_eq!(task.id, "t1");
    assert_eq!(task.root_id, "t1");
    assert_eq!(task.parent_id, None);
    assert_eq!(task.depth, 0);
    assert_eq!(task.state, Backlog);
    assert_eq!(task.repo_root, "/repo");
    assert_eq!(task.title, "Title");
    assert_eq!(task.spec, spec());
    assert_eq!(task.priority, Priority::P1);
    assert_eq!((task.created_at, task.updated_at), (100, 100));
    assert!(task.context_notes.is_empty());
    assert_eq!(task.fingerprint, None);
    assert_eq!(task.blocked_reason, None);
    assert_eq!(task.unblock_cycles, 0);
}

#[test]
fn subtask_inherits_root_repo_and_priority_and_starts_ready() {
    let mut root = Task::new_root("root", "/repo", "Root", spec(), Priority::P0, 100);
    root.context_notes.push("parent only".to_string());
    root.unblock_cycles = 4;
    root.fingerprint = Some("fp".to_string());
    let child_spec = ImmutableSpec {
        description: "part one".to_string(),
        acceptance: Vec::new(),
    };
    let child = Task::new_subtask("child", &root, "Child", child_spec.clone(), 200).unwrap();

    assert_eq!(child.id, "child");
    assert_eq!(child.root_id, "root");
    assert_eq!(child.parent_id.as_deref(), Some("root"));
    assert_eq!(child.repo_root, "/repo");
    assert_eq!(child.priority, Priority::P0);
    assert_eq!(child.depth, 1);
    assert_eq!(child.state, Ready);
    assert_eq!(child.title, "Child");
    assert_eq!(child.spec, child_spec);
    assert_eq!((child.created_at, child.updated_at), (200, 200));
    // Per-task progress is not inherited.
    assert!(child.context_notes.is_empty());
    assert_eq!(child.unblock_cycles, 0);
    assert_eq!(child.fingerprint, None);
    assert_eq!(child.blocked_reason, None);
}

#[test]
fn a_subtask_cannot_spawn_a_subtask() {
    let root = Task::new_root("root", "/repo", "Root", spec(), Priority::P2, 1);
    let child = Task::new_subtask("child", &root, "Child", spec(), 2).unwrap();
    assert_eq!(child.depth, MAX_SUBTASK_DEPTH);
    let err = Task::new_subtask("grandchild", &child, "No", spec(), 3).unwrap_err();
    assert_eq!(err, DomainError::SubtaskDepthExceeded { depth: 2, max: 1 });
    assert_eq!(err.to_string(), "subtask depth 2 exceeds the maximum of 1");
}

#[test]
fn subtask_depth_saturates_instead_of_wrapping() {
    let mut deep = Task::new_root("root", "/repo", "Root", spec(), Priority::P2, 1);
    deep.depth = u8::MAX;
    let err = Task::new_subtask("x", &deep, "No", spec(), 2).unwrap_err();
    assert_eq!(
        err,
        DomainError::SubtaskDepthExceeded { depth: 255, max: 1 }
    );
}

#[test]
fn priority_sorts_most_urgent_first() {
    let mut all = vec![Priority::P3, Priority::P1, Priority::P0, Priority::P2];
    all.sort();
    assert_eq!(
        all,
        vec![Priority::P0, Priority::P1, Priority::P2, Priority::P3]
    );
}
