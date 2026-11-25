#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use aaf_domain::ports::{EventSink, KvStore, RunStore, TaskStore};
use aaf_domain::task::{ImmutableSpec, Priority, Task};

fn make_task(id: &str, repo: &str) -> Task {
    Task::new_root(
        id,
        repo,
        format!("title {id}"),
        ImmutableSpec {
            description: "desc".into(),
            acceptance: vec!["ok".into()],
        },
        Priority::P2,
        100,
    )
}

fn make_run(
    id: &str,
    task_id: &str,
    account_id: Option<&str>,
    status: RunStatus,
    started_at: i64,
    tokens: (u64, u64),
) -> RunRecord {
    RunRecord {
        id: id.into(),
        task_id: task_id.into(),
        account_id: account_id.map(str::to_string),
        executor: "claude".into(),
        worktree_path: String::new(),
        branch: "b".into(),
        status,
        exit_code: None,
        tokens_in: tokens.0,
        tokens_out: tokens.1,
        started_at,
        ended_at: None,
    }
}

#[test]
fn board_enrichment_batches_frozen_deps_and_latest_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    db.set_meta(META_FACTORY_FROZEN, "1").unwrap();
    db.insert_task(&make_task("t1", "/repo-a")).unwrap();
    db.insert_task(&make_task("t2", "/repo-a")).unwrap();
    db.insert_task(&make_task("t3", "/repo-a")).unwrap();
    db.insert_task(&make_task("tb", "/repo-b")).unwrap();
    // Insert deps in non-alphabetical order to confirm rowid preservation.
    db.add_dep("t1", "t3").unwrap();
    db.add_dep("t1", "t2").unwrap();
    db.insert_run(&make_run(
        "r-a",
        "t1",
        Some("acc1"),
        RunStatus::Succeeded,
        200,
        (10, 5),
    ))
    .unwrap();
    db.insert_run(&make_run(
        "r-z",
        "t1",
        Some("acc1"),
        RunStatus::Failed,
        200,
        (1, 1),
    ))
    .unwrap();
    db.insert_run(&make_run(
        "r-b",
        "tb",
        Some("acc2"),
        RunStatus::Running,
        300,
        (50, 25),
    ))
    .unwrap();

    let scoped = db.board_enrichment(Some("/repo-a")).unwrap();
    assert!(scoped.frozen);
    assert_eq!(
        scoped.deps_by_task.get("t1").map(Vec::as_slice),
        Some(&["t3".to_string(), "t2".to_string()][..])
    );
    // Same started_at -> lexicographically larger id wins, matching max_by_key.
    assert_eq!(
        scoped.latest_run_by_task.get("t1").map(String::as_str),
        Some("r-z")
    );
    let t1_sum = scoped.run_summary_by_task.get("t1").unwrap();
    assert_eq!(t1_sum.id, "r-z");
    assert_eq!(t1_sum.run_count, 2);
    assert_eq!(t1_sum.executor, "claude");
    assert_eq!(t1_sum.started_at, 200);
    assert!(!scoped.latest_run_by_task.contains_key("tb"));

    let all = db.board_enrichment(None).unwrap();
    assert_eq!(
        all.latest_run_by_task.get("tb").map(String::as_str),
        Some("r-b")
    );
    assert_eq!(all.run_summary_by_task.get("tb").unwrap().run_count, 1);
}

#[test]
fn running_runs_recent_events_and_tokens_since_all_respect_scope_and_window() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("aaf.db")).unwrap();
    db.insert_task(&make_task("ta", "/repo-a")).unwrap();
    db.insert_task(&make_task("tb", "/repo-b")).unwrap();
    db.insert_run(&make_run(
        "r1",
        "ta",
        Some("acc1"),
        RunStatus::Running,
        500,
        (100, 50),
    ))
    .unwrap();
    db.insert_run(&make_run(
        "r2",
        "ta",
        Some("acc1"),
        RunStatus::Succeeded,
        100,
        (999, 1),
    ))
    .unwrap();
    db.insert_run(&make_run(
        "r3",
        "tb",
        Some("acc2"),
        RunStatus::Running,
        600,
        (40, 60),
    ))
    .unwrap();

    let run_a = db.running_runs(Some("/repo-a")).unwrap();
    assert_eq!(run_a.len(), 1);
    assert_eq!(run_a[0].id, "r1");
    let run_all = db.running_runs(None).unwrap();
    assert_eq!(run_all.len(), 2);

    let tokens = db.tokens_since_all(400).unwrap();
    assert_eq!(tokens.get("acc1"), Some(&150));
    assert_eq!(tokens.get("acc2"), Some(&100));

    db.emit(&FactoryEvent::new(EventKind::TaskCreated, "{}", 10).for_task("ta"))
        .unwrap();
    db.emit(&FactoryEvent::new(EventKind::TaskStateChanged, "{}", 30).for_task("ta"))
        .unwrap();
    db.emit(&FactoryEvent::new(EventKind::TaskCreated, "{}", 20).for_task("tb"))
        .unwrap();

    let ev_a = db.recent_events(Some("/repo-a"), 1).unwrap();
    assert_eq!(ev_a.len(), 1);
    assert_eq!(ev_a[0].created_at, 30);
    let ev_all = db.recent_events(None, 2).unwrap();
    assert_eq!(ev_all.len(), 2);
    assert_eq!(ev_all[0].created_at, 30);
    assert_eq!(ev_all[1].created_at, 20);
}
