#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `TaskState::Adopting` isolation at the store level: the by-id write paths
//! refuse an adopted PR and leave the store byte-for-byte as it was, the
//! counters and latest-event queries count it, and the generic store moves
//! cannot reach it. The scheduler-side paths (poll_gating, merge_retry,
//! sweep_review, recover) are in `src/factory/tests/adopt_isolation.rs`; the
//! CLI-side refusals are in `crates/aaf-cli/tests/adopt_isolation.rs`.

use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{
    EventSink, FingerprintStore, KvStore, META_CONSECUTIVE_ROOT_FAILS, PortError, RunStore,
    TaskStore,
};
use aaf_domain::run::{RunRecord, RunStatus};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::{CancelRefusal, Db, DoneOptions, DoneRefusal};
use aaf_engine::factory::snapshots::steer_task;
use aaf_engine::telemetry::{CostPolicy, StatusQuery, collect_status_from_db};
use tempfile::TempDir;

const REPO: &str = "/repo";

fn open() -> (TempDir, Db) {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    (tmp, db)
}

fn task(id: &str, state: TaskState) -> Task {
    let spec = ImmutableSpec {
        description: "spec".into(),
        acceptance: vec!["ok".into()],
    };
    let mut task = Task::new_root(id, REPO, id, spec, Priority::P2, 10);
    task.state = state;
    task.fingerprint = Some(format!("fp-{id}"));
    task
}

/// The adopted task, with bait for every state-blind path: a fingerprint, an
/// elapsed retry deadline, a running run and PR events.
fn adopting(db: &Db, id: &str) -> Task {
    let mut t = task(id, TaskState::Adopting);
    t.retry_after = Some(5);
    db.insert_task(&t).unwrap();
    db.insert_run(&RunRecord {
        id: format!("run-{id}"),
        task_id: id.to_string(),
        account_id: None,
        executor: "fake".into(),
        worktree_path: "/wt".into(),
        branch: format!("aaf/{id}"),
        status: RunStatus::Running,
        exit_code: None,
        tokens_in: 0,
        tokens_out: 0,
        started_at: 11,
        ended_at: None,
    })
    .unwrap();
    for (kind, payload) in [
        (EventKind::PrAdopted, r#"{"pr_number":7}"#),
        (EventKind::AdoptStep, r#"{"phase":"verify","attempt":1}"#),
    ] {
        db.emit(&FactoryEvent::new(kind, payload.to_string(), 12).for_task(id))
            .unwrap();
    }
    t
}

/// Everything a refused command must leave alone.
#[derive(Debug, PartialEq)]
struct Snapshot {
    state: TaskState,
    updated_at: i64,
    closed_fingerprint: bool,
    root_fails: Option<String>,
    events: usize,
    deps: Vec<(String, String)>,
    notes: Vec<String>,
}

fn snapshot(db: &Db, id: &str) -> Snapshot {
    let t = db.get_task(id).unwrap();
    Snapshot {
        state: t.state,
        updated_at: t.updated_at,
        closed_fingerprint: db
            .find_closed_since(&format!("fp-{id}"), REPO, 0)
            .unwrap()
            .is_some(),
        root_fails: db.get_meta(META_CONSECUTIVE_ROOT_FAILS).unwrap(),
        events: db.events_for_task(id, 100).unwrap().len(),
        deps: db.all_deps().unwrap(),
        notes: t.context_notes,
    }
}

#[test]
fn board_done_refuses_adopting_even_with_force_and_changes_nothing() {
    let (_tmp, db) = open();
    adopting(&db, "t-ad");
    db.set_meta(META_CONSECUTIVE_ROOT_FAILS, "3").unwrap();
    let before = snapshot(&db, "t-ad");
    assert_eq!(before.root_fails.as_deref(), Some("3"));
    assert!(!before.closed_fingerprint);

    for force in [false, true] {
        let opts = DoneOptions {
            reason: "merged by hand",
            pr_number: Some(7),
            commit_sha: Some("abcdef1"),
            force,
        };
        let refusal = db.complete_task_manual_atomic("t-ad", &opts, 99).unwrap();
        assert_eq!(
            refusal,
            Err(DoneRefusal::ActiveState {
                task_id: "t-ad".into(),
                state: TaskState::Adopting
            }),
            "force={force}"
        );
        assert_eq!(snapshot(&db, "t-ad"), before, "force={force}");
    }
    // The counter reset and the fingerprint close are exactly what a Done would do.
    assert_eq!(db.get_task("t-ad").unwrap().state, TaskState::Adopting);
}

#[test]
fn board_done_refuses_a_parent_with_an_adopting_subtask_even_with_force() {
    let (_tmp, db) = open();
    db.insert_task(&task("t-p", TaskState::Review)).unwrap();
    let mut child = task("t-c", TaskState::Adopting);
    child.parent_id = Some("t-p".into());
    child.root_id = "t-p".into();
    child.depth = 1;
    db.insert_task(&child).unwrap();
    db.set_meta(META_CONSECUTIVE_ROOT_FAILS, "2").unwrap();
    let before = snapshot(&db, "t-p");

    let opts = DoneOptions {
        reason: "merged",
        pr_number: None,
        commit_sha: None,
        force: true,
    };
    let refusal = db.complete_task_manual_atomic("t-p", &opts, 99).unwrap();
    assert_eq!(
        refusal,
        Err(DoneRefusal::ActiveDescendant {
            child_id: "t-c".into(),
            state: TaskState::Adopting
        })
    );
    assert_eq!(snapshot(&db, "t-p"), before);
    assert_eq!(db.get_task("t-c").unwrap().state, TaskState::Adopting);
}

#[test]
fn dep_add_refuses_when_either_end_is_adopting() {
    let (_tmp, db) = open();
    adopting(&db, "t-ad");
    db.insert_task(&task("t-ready", TaskState::Ready)).unwrap();
    let before = snapshot(&db, "t-ad");

    for (task_id, blocked_by) in [("t-ready", "t-ad"), ("t-ad", "t-ready")] {
        let err = db.add_dep(task_id, blocked_by).unwrap_err();
        assert!(
            matches!(&err, PortError::Conflict(msg) if msg.contains("adopting")),
            "{task_id} -> {blocked_by}: {err:?}"
        );
    }
    assert!(db.all_deps().unwrap().is_empty());
    assert_eq!(snapshot(&db, "t-ad"), before);

    // The neighbour: two ordinary tasks still get their edge.
    db.insert_task(&task("t-other", TaskState::Ready)).unwrap();
    db.add_dep("t-other", "t-ready").unwrap();
    assert_eq!(
        db.all_deps().unwrap(),
        vec![("t-other".to_string(), "t-ready".to_string())]
    );
}

#[test]
fn dep_rm_refuses_when_either_end_is_adopting_and_keeps_the_edge() {
    let (tmp, db) = open();
    adopting(&db, "t-ad");
    db.insert_task(&task("t-ready", TaskState::Ready)).unwrap();
    // The public API cannot build an edge onto an Adopting task (that is the
    // point), so plant both directions the way an older store could hold them.
    {
        let raw = rusqlite::Connection::open(tmp.path().join("aaf.db")).unwrap();
        for (a, b) in [("t-ready", "t-ad"), ("t-ad", "t-ready")] {
            raw.execute(
                "INSERT INTO task_deps (task_id, blocked_by) VALUES (?1, ?2)",
                [a, b],
            )
            .unwrap();
        }
    }
    let planted = db.all_deps().unwrap();
    assert_eq!(planted.len(), 2);

    for (task_id, blocked_by) in [("t-ready", "t-ad"), ("t-ad", "t-ready")] {
        let err = db.remove_dep(task_id, blocked_by).unwrap_err();
        assert!(
            matches!(&err, PortError::Conflict(msg) if msg.contains("adopting")),
            "{task_id} -> {blocked_by}: {err:?}"
        );
    }
    assert_eq!(db.all_deps().unwrap(), planted);

    // The neighbour: an edge between ordinary tasks is still removable, and a
    // missing edge is still "not present" rather than an error.
    db.insert_task(&task("t-x", TaskState::Ready)).unwrap();
    db.add_dep("t-x", "t-ready").unwrap();
    assert!(db.remove_dep("t-x", "t-ready").unwrap());
    assert!(!db.remove_dep("t-x", "t-ready").unwrap());
}

#[test]
fn steer_refuses_adopting_and_leaves_the_notes_alone() {
    let (_tmp, db) = open();
    adopting(&db, "t-ad");
    db.insert_task(&task("t-ready", TaskState::Ready)).unwrap();
    let before = snapshot(&db, "t-ad");

    let err = steer_task(&db, "t-ad", "please also fix the typo", 99).unwrap_err();
    assert!(err.contains("adopting"), "{err}");
    assert_eq!(snapshot(&db, "t-ad"), before);
    assert!(before.notes.is_empty());

    // The neighbour: an ordinary task still takes a steering note.
    let note = steer_task(&db, "t-ready", "prefer the small fix", 99).unwrap();
    assert_eq!(note, "steer: prefer the small fix");
    assert_eq!(db.get_task("t-ready").unwrap().context_notes, vec![note]);
}

#[test]
fn board_cancel_still_works_on_adopting_without_a_running_run() {
    let (_tmp, db) = open();
    // No bait run here: a lane that holds a `running` run is refused below.
    let mut t = task("t-ad", TaskState::Adopting);
    t.retry_after = Some(5);
    db.insert_task(&t).unwrap();
    db.set_meta(META_CONSECUTIVE_ROOT_FAILS, "3").unwrap();

    let out = db
        .cancel_task_atomic("t-ad", false, Some("chat pushed a fix"), 99)
        .unwrap()
        .unwrap();
    assert_eq!(out.cancelled_ids, vec!["t-ad".to_string()]);
    let after = db.get_task("t-ad").unwrap();
    assert_eq!(after.state, TaskState::Cancelled);
    // Cancelling an adopted PR is not a failed root and closes no fingerprint.
    assert_eq!(
        db.get_meta(META_CONSECUTIVE_ROOT_FAILS).unwrap().as_deref(),
        Some("3")
    );
    assert!(
        db.find_closed_since("fp-t-ad", REPO, 0).unwrap().is_none(),
        "cancel recorded a closed fingerprint"
    );
}

#[test]
fn board_cancel_refuses_adopting_while_the_lane_holds_a_running_run() {
    let (_tmp, db) = open();
    adopting(&db, "t-ad");
    let before = snapshot(&db, "t-ad");

    let refusal = db.cancel_task_atomic("t-ad", false, None, 99).unwrap();
    assert!(
        matches!(refusal, Err(CancelRefusal::RunningRun { ref task_id, .. }) if task_id == "t-ad"),
        "{refusal:?}"
    );
    assert_eq!(snapshot(&db, "t-ad"), before);
}

#[test]
fn nothing_moves_into_adopting_and_only_done_or_cancelled_leave_it() {
    let (_tmp, db) = open();
    adopting(&db, "t-ad");
    for target in [
        TaskState::Ready,
        TaskState::Running,
        TaskState::Gating,
        TaskState::Review,
        TaskState::Blocked,
        TaskState::Backlog,
    ] {
        let err = db.set_state("t-ad", target, None, 99).unwrap_err();
        assert!(matches!(err, PortError::Invalid(_)), "{target:?}: {err:?}");
        let err = db
            .compare_and_set_state("t-ad", TaskState::Adopting, target, None, 99)
            .unwrap_err();
        assert!(matches!(err, PortError::Invalid(_)), "{target:?}: {err:?}");
    }
    assert_eq!(db.get_task("t-ad").unwrap().state, TaskState::Adopting);

    // No state moves into Adopting: tasks are inserted in it.
    db.insert_task(&task("t-ready", TaskState::Ready)).unwrap();
    let err = db
        .set_state("t-ready", TaskState::Adopting, None, 99)
        .unwrap_err();
    assert!(matches!(err, PortError::Invalid(_)), "{err:?}");
    assert_eq!(db.get_task("t-ready").unwrap().state, TaskState::Ready);
}

#[test]
fn wake_due_blocked_tasks_does_not_wake_an_adopting_task() {
    let (_tmp, db) = open();
    // `retry_after` is long past: a state-blind wake would pick it up.
    let adopted = adopting(&db, "t-ad");
    assert_eq!(
        db.get_task("t-ad").unwrap().retry_after,
        adopted.retry_after
    );
    let mut blocked = task("t-blk", TaskState::Blocked);
    blocked.retry_after = Some(5);
    blocked.blocked_reason = Some(aaf_domain::blocked::BlockedReason::Infra {
        detail: "net".into(),
    });
    db.insert_task(&blocked).unwrap();
    let before = snapshot(&db, "t-ad");

    let woken = db.wake_due_blocked_tasks(REPO, 1_000_000).unwrap();
    assert_eq!(
        woken,
        vec!["t-blk".to_string()],
        "only the blocked task wakes"
    );
    assert_eq!(snapshot(&db, "t-ad"), before);
    assert_eq!(db.get_task("t-blk").unwrap().state, TaskState::Ready);
}

#[test]
fn latest_event_and_status_counts_include_adopting() {
    let (tmp, db) = open();
    adopting(&db, "t-ad");
    db.insert_task(&task("t-done", TaskState::Ready)).unwrap();

    // Board enrichment: a Running/Gating-only filter left an adopted PR without
    // its "what is it doing" event.
    for repo in [Some(REPO), None] {
        let enrichment = db.board_enrichment(repo).unwrap();
        let latest = enrichment
            .latest_event_by_task
            .get("t-ad")
            .unwrap_or_else(|| panic!("no latest event for the adopting task (repo {repo:?})"));
        assert_eq!(latest.0, EventKind::AdoptStep.as_str());
    }

    let summary = collect_status_from_db(
        &db,
        &StatusQuery {
            repo_root: Some(REPO),
            data_dir: Some(tmp.path()),
            now: 100,
            cost: &CostPolicy::default(),
        },
    )
    .unwrap();
    assert_eq!(summary.counts.adopting, 1);
    assert_eq!(summary.counts.ready, 1);
    assert_eq!(summary.counts.total, 2);
    // Not mislabeled as blocked, running, gating or done.
    assert_eq!(
        (
            summary.counts.blocked,
            summary.counts.running,
            summary.counts.gating,
            summary.counts.done
        ),
        (0, 0, 0, 0)
    );
}

#[test]
fn repo_task_summary_counts_adopting_as_active_work() {
    let (_tmp, db) = open();
    adopting(&db, "t-ad");
    let rows = db.repo_task_summary(Some(REPO)).unwrap();
    assert_eq!(rows, vec![(REPO.to_string(), 1, 0)]);
}
