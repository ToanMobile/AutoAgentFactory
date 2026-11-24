#![allow(clippy::unwrap_used, clippy::expect_used)]
//! A13 O13-2: `collect_status_from_db` counts tasks with one grouped query
//! instead of decoding every task row, with the same result as the generic
//! per-task path.

use aaf_domain::blocked::BlockedReason;
use aaf_domain::ports::TaskStore;
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::telemetry::{CostRates, StatusQuery, collect_status, collect_status_from_db};
use rusqlite::Connection;

fn task(id: &str, repo: &str, state: TaskState, reason: Option<BlockedReason>) -> Task {
    let mut t = Task::new_root(
        id,
        repo,
        id,
        ImmutableSpec {
            description: "d".to_string(),
            acceptance: vec![],
        },
        Priority::P2,
        1,
    );
    t.state = state;
    t.blocked_reason = reason;
    t
}

#[test]
fn grouped_status_counts_match_per_task_decode_and_skip_unrelated_columns() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aaf.db");
    let db = Db::open(&path).unwrap();
    let infra = |d: &str| BlockedReason::Infra {
        detail: d.to_string(),
    };
    let rows = [
        task("a1", "/a", TaskState::Ready, None),
        task("a2", "/a", TaskState::Blocked, Some(infra("x"))),
        task("a3", "/a", TaskState::Blocked, Some(infra("y"))),
        task(
            "a4",
            "/a",
            TaskState::Blocked,
            Some(BlockedReason::Quota { reset_at: Some(9) }),
        ),
        task("a5", "/a", TaskState::Blocked, None),
        task("a6", "/a", TaskState::Done, None),
        task("b1", "/b", TaskState::Blocked, Some(infra("z"))),
        task("b2", "/b", TaskState::Review, None),
    ];
    for t in &rows {
        db.insert_task(t).unwrap();
    }
    for repo in [None, Some("/a"), Some("/b"), Some("/none")] {
        let query = StatusQuery {
            repo_root: repo,
            data_dir: None,
            now: 10,
            rates: CostRates::default(),
        };
        let generic = collect_status(&db, &db, &db, Some(&db), &query).unwrap();
        let grouped = collect_status_from_db(&db, &query).unwrap();
        assert_eq!(generic, grouped, "repo={repo:?}");
    }
    let all = StatusQuery {
        repo_root: Some("/a"),
        data_dir: None,
        now: 10,
        rates: CostRates::default(),
    };
    let before = collect_status_from_db(&db, &all).unwrap();
    assert_eq!(before.counts.blocked, 4);
    assert_eq!(before.blocked_reasons.get("infra"), Some(&2));
    assert_eq!(before.blocked_reasons.get("quota"), Some(&1));
    assert_eq!(before.blocked_reasons.get("unknown"), Some(&1));

    // Counting never needs the spec/notes blobs: an unreadable notes column on
    // one task must not take the whole status view down.
    let raw = Connection::open(&path).unwrap();
    raw.execute(
        "UPDATE tasks SET context_notes = 'not json' WHERE id = 'a1'",
        [],
    )
    .unwrap();
    drop(raw);
    let after = collect_status_from_db(&db, &all).unwrap();
    assert_eq!(after, before);
}
