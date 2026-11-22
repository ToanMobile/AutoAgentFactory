#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `TaskStore` on a real SQLite file.

use aaf_domain::blocked::BlockedReason;
use aaf_domain::ports::{PortError, TaskStore};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use tempfile::TempDir;

fn open() -> (TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("aaf.db")).unwrap();
    (dir, db)
}

fn spec() -> ImmutableSpec {
    ImmutableSpec {
        description: "fix the crash; it's \"quoted\"".to_string(),
        acceptance: vec!["test passes".to_string(), "no new warnings".to_string()],
    }
}

fn task(id: &str, repo: &str, priority: Priority, state: TaskState, now: i64) -> Task {
    let mut task = Task::new_root(id, repo, format!("title {id}"), spec(), priority, now);
    task.state = state;
    task
}

fn ids(tasks: &[Task]) -> Vec<&str> {
    tasks.iter().map(|t| t.id.as_str()).collect()
}

#[test]
fn insert_and_get_round_trip_every_field() {
    let (_dir, db) = open();
    let parent = task("root", "/repo/a", Priority::P1, TaskState::Running, 100);
    db.insert_task(&parent).unwrap();

    let mut child = Task::new_subtask("child", &parent, "child title", spec(), 150).unwrap();
    child.context_notes = vec!["first".to_string(), "second".to_string()];
    child.state = TaskState::Blocked;
    child.fingerprint = Some("fp-1".to_string());
    child.blocked_reason = Some(BlockedReason::Quota {
        reset_at: Some(999),
    });
    child.unblock_cycles = 3;
    child.updated_at = 160;
    db.insert_task(&child).unwrap();

    assert_eq!(db.get_task("root").unwrap(), parent);
    assert_eq!(db.get_task("child").unwrap(), child);
}

#[test]
fn duplicate_id_conflicts_and_missing_id_is_not_found() {
    let (_dir, db) = open();
    let t = task("t1", "/repo", Priority::P2, TaskState::Backlog, 1);
    db.insert_task(&t).unwrap();
    assert!(matches!(db.insert_task(&t), Err(PortError::Conflict(_))));
    assert!(matches!(db.get_task("nope"), Err(PortError::NotFound(_))));
}

#[test]
fn subtask_of_unknown_parent_is_rejected_by_the_foreign_key() {
    let (_dir, db) = open();
    let mut orphan = task("orphan", "/repo", Priority::P2, TaskState::Ready, 1);
    orphan.parent_id = Some("ghost".to_string());
    assert!(matches!(
        db.insert_task(&orphan),
        Err(PortError::Invalid(_))
    ));
    assert!(matches!(db.get_task("orphan"), Err(PortError::NotFound(_))));
}

#[test]
fn list_orders_by_priority_then_age_and_applies_both_filters() {
    let (_dir, db) = open();
    // Inserted deliberately out of order.
    db.insert_task(&task("p2-old", "/a", Priority::P2, TaskState::Ready, 10))
        .unwrap();
    db.insert_task(&task("p0-new", "/a", Priority::P0, TaskState::Ready, 50))
        .unwrap();
    db.insert_task(&task("p1-b", "/b", Priority::P1, TaskState::Backlog, 20))
        .unwrap();
    db.insert_task(&task("p0-old", "/a", Priority::P0, TaskState::Backlog, 5))
        .unwrap();
    db.insert_task(&task("p2-new", "/b", Priority::P2, TaskState::Ready, 30))
        .unwrap();
    // Same priority and second as p2-new: insertion order breaks the tie.
    db.insert_task(&task("p2-tie", "/b", Priority::P2, TaskState::Ready, 30))
        .unwrap();

    let all = db.list_tasks(None, None).unwrap();
    assert_eq!(
        ids(&all),
        ["p0-old", "p0-new", "p1-b", "p2-old", "p2-new", "p2-tie"]
    );
    let repo_a = db.list_tasks(Some("/a"), None).unwrap();
    assert_eq!(ids(&repo_a), ["p0-old", "p0-new", "p2-old"]);
    let ready = db.list_tasks(None, Some(TaskState::Ready)).unwrap();
    assert_eq!(ids(&ready), ["p0-new", "p2-old", "p2-new", "p2-tie"]);
    let ready_b = db.list_tasks(Some("/b"), Some(TaskState::Ready)).unwrap();
    assert_eq!(ids(&ready_b), ["p2-new", "p2-tie"]);
    assert!(db.list_tasks(Some("/none"), None).unwrap().is_empty());
}

#[test]
fn set_state_follows_the_lifecycle_and_stores_blocked_only_when_blocked() {
    let (_dir, db) = open();
    db.insert_task(&task("t", "/r", Priority::P1, TaskState::Backlog, 1))
        .unwrap();
    let reason = BlockedReason::GateFailed {
        summary: "2 tests failed".to_string(),
    };

    // A reason passed with a non-blocked state is not stored.
    db.set_state("t", TaskState::Ready, Some(&reason), 2)
        .unwrap();
    let t = db.get_task("t").unwrap();
    assert_eq!(
        (t.state, t.blocked_reason, t.updated_at),
        (TaskState::Ready, None, 2)
    );

    db.set_state("t", TaskState::Blocked, Some(&reason), 3)
        .unwrap();
    let t = db.get_task("t").unwrap();
    assert_eq!(t.state, TaskState::Blocked);
    assert_eq!(t.blocked_reason, Some(reason));
    assert_eq!((t.created_at, t.updated_at), (1, 3));

    db.set_state("t", TaskState::Ready, None, 4).unwrap();
    let t = db.get_task("t").unwrap();
    assert_eq!((t.state, t.blocked_reason), (TaskState::Ready, None));
}

#[test]
fn set_state_rejects_a_forbidden_move_and_leaves_the_task_untouched() {
    let (_dir, db) = open();
    let original = task("t", "/r", Priority::P1, TaskState::Backlog, 1);
    db.insert_task(&original).unwrap();
    let err = db.set_state("t", TaskState::Done, None, 9).unwrap_err();
    assert!(matches!(err, PortError::Invalid(_)), "{err:?}");
    assert_eq!(db.get_task("t").unwrap(), original);
    assert!(matches!(
        db.set_state("ghost", TaskState::Ready, None, 9),
        Err(PortError::NotFound(_))
    ));
}

#[test]
fn set_state_agrees_with_can_transition_to_for_every_pair() {
    let (_dir, db) = open();
    for from in TaskState::ALL {
        for to in TaskState::ALL {
            let id = format!("{}-{}", from.as_str(), to.as_str());
            db.insert_task(&task(&id, "/r", Priority::P3, from, 1))
                .unwrap();
            let result = db.set_state(&id, to, None, 2);
            let stored = db.get_task(&id).unwrap().state;
            if from.can_transition_to(to) {
                assert_eq!(result, Ok(()), "{id}");
                assert_eq!(stored, to, "{id}");
            } else {
                assert!(matches!(result, Err(PortError::Invalid(_))), "{id}");
                assert_eq!(stored, from, "{id}");
            }
        }
    }
}

#[test]
fn context_notes_append_in_order_without_touching_the_spec() {
    let (_dir, db) = open();
    db.insert_task(&task("t", "/r", Priority::P1, TaskState::Ready, 1))
        .unwrap();
    db.append_context_note("t", "note one", 5).unwrap();
    db.append_context_note("t", "note 'two' \u{1F600}", 6)
        .unwrap();
    let t = db.get_task("t").unwrap();
    assert_eq!(t.context_notes, ["note one", "note 'two' \u{1F600}"]);
    assert_eq!(t.spec, spec());
    assert_eq!(t.updated_at, 6);
    assert!(matches!(
        db.append_context_note("ghost", "x", 1),
        Err(PortError::NotFound(_))
    ));
}

#[test]
fn increment_unblock_cycles_returns_the_new_count() {
    let (_dir, db) = open();
    db.insert_task(&task("t", "/r", Priority::P1, TaskState::Ready, 1))
        .unwrap();
    assert_eq!(db.increment_unblock_cycles("t", 2).unwrap(), 1);
    assert_eq!(db.increment_unblock_cycles("t", 3).unwrap(), 2);
    let t = db.get_task("t").unwrap();
    assert_eq!((t.unblock_cycles, t.updated_at), (2, 3));
    assert!(matches!(
        db.increment_unblock_cycles("ghost", 1),
        Err(PortError::NotFound(_))
    ));
}

#[test]
fn deps_round_trip_and_cycles_are_rejected() {
    let (_dir, db) = open();
    for id in ["a", "b", "c", "d"] {
        db.insert_task(&task(id, "/r", Priority::P1, TaskState::Backlog, 1))
            .unwrap();
    }
    db.add_dep("a", "b").unwrap();
    db.add_dep("b", "c").unwrap();
    db.add_dep("a", "d").unwrap();
    // Adding the same edge again is a no-op.
    db.add_dep("a", "b").unwrap();
    assert_eq!(db.deps_of("a").unwrap(), ["b", "d"]);
    assert_eq!(db.deps_of("b").unwrap(), ["c"]);
    assert!(db.deps_of("c").unwrap().is_empty());

    // Forward edge over an existing path is fine.
    db.add_dep("a", "c").unwrap();

    for (task_id, blocked_by) in [("a", "a"), ("b", "a"), ("c", "a")] {
        let err = db.add_dep(task_id, blocked_by).unwrap_err();
        assert!(
            matches!(err, PortError::Conflict(_)),
            "{task_id}->{blocked_by}: {err:?}"
        );
    }
    // A rejected edge is not stored.
    assert!(db.deps_of("c").unwrap().is_empty());
    assert_eq!(db.deps_of("b").unwrap(), ["c"]);
}

#[test]
fn deps_between_unknown_tasks_are_not_found() {
    let (_dir, db) = open();
    db.insert_task(&task("a", "/r", Priority::P1, TaskState::Backlog, 1))
        .unwrap();
    assert!(matches!(
        db.add_dep("a", "ghost"),
        Err(PortError::NotFound(_))
    ));
    assert!(matches!(
        db.add_dep("ghost", "a"),
        Err(PortError::NotFound(_))
    ));
    assert!(db.deps_of("a").unwrap().is_empty());
}

#[test]
fn children_of_lists_only_direct_children_in_list_order() {
    let (_dir, db) = open();
    let root = task("root", "/r", Priority::P1, TaskState::Running, 1);
    let other = task("other", "/r", Priority::P1, TaskState::Running, 1);
    db.insert_task(&root).unwrap();
    db.insert_task(&other).unwrap();
    let late = Task::new_subtask("late", &root, "late", spec(), 30).unwrap();
    let early = Task::new_subtask("early", &root, "early", spec(), 20).unwrap();
    let foreign = Task::new_subtask("foreign", &other, "foreign", spec(), 10).unwrap();
    for t in [&late, &early, &foreign] {
        db.insert_task(t).unwrap();
    }
    assert_eq!(ids(&db.children_of("root").unwrap()), ["early", "late"]);
    assert!(db.children_of("early").unwrap().is_empty());
}
