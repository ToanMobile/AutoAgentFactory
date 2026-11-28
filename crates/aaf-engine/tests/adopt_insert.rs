#![allow(clippy::unwrap_used, clippy::expect_used)]

//! `Db::insert_adopting`: the one transaction that turns a PR into a task. A
//! second live adoption of the same PR in the same repository is reported and
//! writes nothing; finished ones and other repositories do not count; a store
//! that fails half-way leaves neither the task nor an event.

use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{EventSink, PortError, TaskStore};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::{AdoptInsert, Db};

fn adopting(id: &str, repo: &str, fingerprint: &str) -> Task {
    let spec = ImmutableSpec {
        description: "adopted".into(),
        acceptance: vec!["merged".into()],
    };
    let mut task = Task::new_root(id, repo, "adopt PR", spec, Priority::P1, 10);
    task.state = TaskState::Adopting;
    task.fingerprint = Some(fingerprint.into());
    task
}

fn events(id: &str) -> Vec<FactoryEvent> {
    vec![
        FactoryEvent::new(EventKind::TaskCreated, "{}", 11).for_task(id),
        FactoryEvent::new(EventKind::PrAdopted, "{}", 11).for_task(id),
    ]
}

fn open() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("aaf.db")).unwrap();
    (dir, db)
}

#[test]
fn the_task_and_both_events_go_in_together() {
    let (_dir, db) = open();
    let task = adopting("t-a", "/repo", "gh-pr:org/repo#7");
    assert_eq!(
        db.insert_adopting(&task, &events("t-a")).unwrap(),
        AdoptInsert::Inserted
    );
    assert_eq!(db.get_task("t-a").unwrap().state, TaskState::Adopting);
    assert_eq!(db.events_for_task("t-a", 10).unwrap().len(), 2);
}

#[test]
fn a_live_adoption_of_the_same_pr_in_the_same_repo_is_reported_and_writes_nothing() {
    let (_dir, db) = open();
    db.insert_adopting(
        &adopting("t-a", "/repo", "gh-pr:org/repo#7"),
        &events("t-a"),
    )
    .unwrap();
    let again = db
        .insert_adopting(
            &adopting("t-b", "/repo", "gh-pr:org/repo#7"),
            &events("t-b"),
        )
        .unwrap();
    assert_eq!(
        again,
        AdoptInsert::AlreadyAdopting {
            task_id: "t-a".into()
        }
    );
    assert!(matches!(db.get_task("t-b"), Err(PortError::NotFound(_))));
    assert!(db.events_for_task("t-b", 10).unwrap().is_empty());
}

#[test]
fn other_prs_and_other_repos_do_not_count() {
    let (_dir, db) = open();
    db.insert_adopting(
        &adopting("t-a", "/repo", "gh-pr:org/repo#7"),
        &events("t-a"),
    )
    .unwrap();
    for (id, repo, fp) in [
        ("t-b", "/repo", "gh-pr:org/repo#8"),
        ("t-c", "/other", "gh-pr:org/repo#7"),
    ] {
        assert_eq!(
            db.insert_adopting(&adopting(id, repo, fp), &events(id))
                .unwrap(),
            AdoptInsert::Inserted,
            "{id}"
        );
    }
}

#[test]
fn a_done_or_cancelled_adoption_does_not_block_a_new_one() {
    for end in [TaskState::Done, TaskState::Cancelled] {
        let (_dir, db) = open();
        db.insert_adopting(
            &adopting("t-a", "/repo", "gh-pr:org/repo#7"),
            &events("t-a"),
        )
        .unwrap();
        assert!(
            db.compare_and_set_state("t-a", TaskState::Adopting, end, None, 20)
                .unwrap()
        );
        assert_eq!(
            db.insert_adopting(
                &adopting("t-b", "/repo", "gh-pr:org/repo#7"),
                &events("t-b")
            )
            .unwrap(),
            AdoptInsert::Inserted,
            "{end:?}"
        );
    }
}

#[test]
fn only_an_adopting_task_with_a_fingerprint_is_accepted() {
    let (_dir, db) = open();
    let mut ready = adopting("t-a", "/repo", "gh-pr:org/repo#7");
    ready.state = TaskState::Ready;
    assert!(matches!(
        db.insert_adopting(&ready, &events("t-a")),
        Err(PortError::Invalid(_))
    ));
    let mut bare = adopting("t-b", "/repo", "x");
    bare.fingerprint = None;
    assert!(matches!(
        db.insert_adopting(&bare, &events("t-b")),
        Err(PortError::Invalid(_))
    ));
    assert!(db.list_tasks(None, None).unwrap().is_empty());
}

#[test]
fn an_event_the_store_refuses_rolls_the_task_back() {
    let (dir, db) = open();
    let conn = rusqlite::Connection::open(dir.path().join("aaf.db")).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER refuse_pr_adopted BEFORE INSERT ON events \
         WHEN NEW.kind = 'pr_adopted' BEGIN SELECT RAISE(ABORT, 'store says no'); END;",
    )
    .unwrap();
    let err = db
        .insert_adopting(
            &adopting("t-a", "/repo", "gh-pr:org/repo#7"),
            &events("t-a"),
        )
        .unwrap_err();
    assert!(err.to_string().contains("store says no"), "{err}");
    assert!(
        !matches!(err, PortError::NotFound(_)),
        "a store failure is an error, not a duplicate"
    );
    assert!(db.list_tasks(None, None).unwrap().is_empty());
    assert!(db.events_for_task("t-a", 10).unwrap().is_empty());
}
