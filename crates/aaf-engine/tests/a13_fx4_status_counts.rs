#![allow(clippy::unwrap_used, clippy::expect_used)]
//! A13 O13-2: `collect_status_from_db` counts tasks with one grouped query
//! instead of decoding every task row, with the same result as the generic
//! per-task path.

use std::collections::BTreeSet;

use aaf_domain::blocked::BlockedReason;
use aaf_domain::ports::{RunStore, TaskStore};
use aaf_domain::run::{RunFinish, RunRecord, RunStatus};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use aaf_engine::telemetry::{
    CostBasis, CostPolicy, CostRates, StatusQuery, collect_status, collect_status_from_db,
};
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
    let default_cost = CostPolicy::default();
    for repo in [None, Some("/a"), Some("/b"), Some("/none")] {
        let query = StatusQuery {
            repo_root: repo,
            data_dir: None,
            now: 10,
            cost: &default_cost,
        };
        let generic = collect_status(&db, &db, &db, Some(&db), &query).unwrap();
        let grouped = collect_status_from_db(&db, &query).unwrap();
        assert_eq!(generic, grouped, "repo={repo:?}");
    }
    let all = StatusQuery {
        repo_root: Some("/a"),
        data_dir: None,
        now: 10,
        cost: &default_cost,
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

#[test]
fn grouped_status_matches_generic_path_with_mixed_api_and_subscription_accounts() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("aaf.db")).unwrap();

    for t in [
        task("a1", "/a", TaskState::Done, None),
        task("a2", "/a", TaskState::Review, None),
        task("b1", "/b", TaskState::Done, None),
    ] {
        db.insert_task(&t).unwrap();
    }

    let add_run = |id: &str, task_id: &str, acct: Option<&str>, t_in: u64, t_out: u64| {
        db.insert_run(&RunRecord {
            id: id.to_string(),
            task_id: task_id.to_string(),
            account_id: acct.map(str::to_string),
            executor: "claude".to_string(),
            worktree_path: "/wt".to_string(),
            branch: "aaf/b".to_string(),
            status: RunStatus::Running,
            exit_code: None,
            tokens_in: 0,
            tokens_out: 0,
            started_at: 10,
            ended_at: None,
        })
        .unwrap();
        db.finish_run(
            id,
            &RunFinish {
                status: RunStatus::Succeeded,
                exit_code: Some(0),
                tokens_in: t_in,
                tokens_out: t_out,
                ended_at: 20,
            },
        )
        .unwrap();
    };

    add_run("r1", "a1", Some("api-1"), 400_000, 100_000);
    add_run("r2", "a2", Some("api-1"), 600_000, 100_000);
    add_run("r3", "a2", Some("sub-1"), 900_000, 300_000);
    add_run("r4", "a2", None, 200_000, 50_000);
    add_run("r5", "b1", Some("sub-1"), 1_000_000, 500_000);

    let policy = CostPolicy {
        rates: CostRates::default(),
        api_accounts: BTreeSet::from(["api-1".to_string()]),
    };
    for repo in [None, Some("/a"), Some("/b"), Some("/none")] {
        let query = StatusQuery {
            repo_root: repo,
            data_dir: None,
            now: 30,
            cost: &policy,
        };
        let generic = collect_status(&db, &db, &db, Some(&db), &query).unwrap();
        let grouped = collect_status_from_db(&db, &query).unwrap();
        assert_eq!(generic, grouped, "repo={repo:?}");
        match repo {
            None | Some("/a") => {
                assert_eq!(grouped.cost_basis, CostBasis::ApiAccounts);
                // api-1 total: 1_000_000 in ($3) + 200_000 out ($3) = $6.00
                assert!((grouped.estimated_cost_usd.unwrap() - 6.0).abs() < 1e-9);
            }
            Some("/b") | Some("/none") => {
                assert_eq!(grouped.cost_basis, CostBasis::None);
                assert_eq!(grouped.estimated_cost_usd, None);
            }
            _ => unreachable!(),
        }
    }
}
