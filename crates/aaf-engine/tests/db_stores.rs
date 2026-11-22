#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Budget, run, account, event, fingerprint and key/value stores on a real
//! SQLite file.

use std::path::Path;

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::budget::RootBudget;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{
    AccountStore, BudgetStore, ClosedFingerprint, EventSink, FingerprintStore, KvStore,
    META_FACTORY_FROZEN, PortError, RunStore, TaskStore,
};
use aaf_domain::run::{RunFinish, RunRecord, RunStatus};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::db::Db;
use tempfile::TempDir;

fn open() -> (TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("aaf.db")).unwrap();
    (dir, db)
}

fn db_path(dir: &TempDir) -> std::path::PathBuf {
    dir.path().join("aaf.db")
}

fn add_task(db: &Db, id: &str, repo: &str, state: TaskState, fingerprint: Option<&str>, now: i64) {
    let spec = ImmutableSpec {
        description: "d".to_string(),
        acceptance: vec![],
    };
    let mut task = Task::new_root(id, repo, "t", spec, Priority::P1, now);
    task.state = state;
    task.fingerprint = fingerprint.map(str::to_string);
    db.insert_task(&task).unwrap();
}

fn budget(root_id: &str) -> RootBudget {
    RootBudget {
        root_id: root_id.to_string(),
        tokens_used: 10,
        token_limit: 500_000,
        wall_secs_used: 20,
        wall_secs_limit: 3600,
        cycles_used: 1,
        cycles_limit: 5,
        frozen: false,
    }
}

fn run(
    id: &str,
    task_id: &str,
    account: Option<&str>,
    started_at: i64,
    tokens: (u64, u64),
) -> RunRecord {
    RunRecord {
        id: id.to_string(),
        task_id: task_id.to_string(),
        account_id: account.map(str::to_string),
        executor: "claude_code".to_string(),
        worktree_path: "/wt/x".to_string(),
        branch: "aaf/x".to_string(),
        status: RunStatus::Running,
        exit_code: None,
        tokens_in: tokens.0,
        tokens_out: tokens.1,
        started_at,
        ended_at: None,
    }
}

fn account(id: &str, dir: &str) -> AccountProfile {
    AccountProfile {
        id: id.to_string(),
        provider: ProviderKind::ClaudeCode,
        config_dir: dir.to_string(),
        name: format!("name of {id}"),
    }
}

#[test]
fn budget_round_trip_overwrite_usage_and_freeze() {
    let (_dir, db) = open();
    assert!(matches!(db.get_budget("r"), Err(PortError::NotFound(_))));
    assert!(matches!(
        db.add_usage("r", 1, 1, 1),
        Err(PortError::NotFound(_))
    ));
    assert!(matches!(
        db.set_frozen("r", true),
        Err(PortError::NotFound(_))
    ));

    let b = budget("r");
    db.put_budget(&b).unwrap();
    assert_eq!(db.get_budget("r").unwrap(), b);

    let updated = db.add_usage("r", 100, 30, 2).unwrap();
    let expected = RootBudget {
        tokens_used: 110,
        wall_secs_used: 50,
        cycles_used: 3,
        ..b.clone()
    };
    assert_eq!(updated, expected);
    assert_eq!(db.get_budget("r").unwrap(), expected);

    db.set_frozen("r", true).unwrap();
    assert!(db.get_budget("r").unwrap().frozen);
    db.set_frozen("r", false).unwrap();
    assert!(!db.get_budget("r").unwrap().frozen);

    // put_budget replaces the whole row.
    let replaced = RootBudget {
        token_limit: 7,
        frozen: true,
        ..b
    };
    db.put_budget(&replaced).unwrap();
    assert_eq!(db.get_budget("r").unwrap(), replaced);
}

#[test]
fn budget_values_sqlite_cannot_hold_are_refused_and_usage_saturates() {
    let (_dir, db) = open();
    let too_big = RootBudget {
        token_limit: u64::MAX,
        ..budget("big")
    };
    assert!(matches!(
        db.put_budget(&too_big),
        Err(PortError::Invalid(_))
    ));
    assert!(matches!(db.get_budget("big"), Err(PortError::NotFound(_))));

    db.put_budget(&budget("r")).unwrap();
    let saturated = db.add_usage("r", u64::MAX, 0, 0).unwrap();
    assert_eq!(saturated.tokens_used, i64::MAX as u64);
    assert_eq!(db.get_budget("r").unwrap(), saturated);
}

#[test]
fn runs_round_trip_finish_and_order() {
    let (_dir, db) = open();
    add_task(&db, "t", "/r", TaskState::Running, None, 1);
    add_task(&db, "other", "/r", TaskState::Running, None, 1);

    let late = run("late", "t", Some("acc"), 200, (0, 0));
    let early = run("early", "t", None, 100, (5, 6));
    db.insert_run(&late).unwrap();
    db.insert_run(&early).unwrap();
    db.insert_run(&run("elsewhere", "other", Some("acc"), 50, (1, 1)))
        .unwrap();
    assert!(matches!(db.insert_run(&late), Err(PortError::Conflict(_))));
    assert_eq!(
        db.runs_for_task("t").unwrap(),
        [early.clone(), late.clone()]
    );

    let finish = RunFinish {
        status: RunStatus::Failed,
        exit_code: Some(-2),
        tokens_in: 1000,
        tokens_out: 234,
        ended_at: 260,
    };
    db.finish_run("late", &finish).unwrap();
    let finished = RunRecord {
        status: RunStatus::Failed,
        exit_code: Some(-2),
        tokens_in: 1000,
        tokens_out: 234,
        ended_at: Some(260),
        ..late
    };
    assert_eq!(db.runs_for_task("t").unwrap(), [early, finished]);
    assert!(matches!(
        db.finish_run("ghost", &finish),
        Err(PortError::NotFound(_))
    ));
    assert!(db.runs_for_task("ghost").unwrap().is_empty());
}

#[test]
fn run_for_an_unknown_task_is_rejected() {
    let (_dir, db) = open();
    let orphan = run("r1", "ghost", None, 1, (0, 0));
    assert!(matches!(db.insert_run(&orphan), Err(PortError::Invalid(_))));
}

#[test]
fn every_run_status_round_trips() {
    let (_dir, db) = open();
    add_task(&db, "t", "/r", TaskState::Running, None, 1);
    for (i, status) in RunStatus::ALL.into_iter().enumerate() {
        let mut record = run(status.as_str(), "t", None, i as i64, (0, 0));
        record.status = status;
        db.insert_run(&record).unwrap();
    }
    let stored: Vec<RunStatus> = db
        .runs_for_task("t")
        .unwrap()
        .into_iter()
        .map(|r| r.status)
        .collect();
    assert_eq!(stored, RunStatus::ALL);
}

#[test]
fn tokens_since_includes_the_boundary_second_and_only_that_account() {
    let (_dir, db) = open();
    add_task(&db, "t", "/r", TaskState::Running, None, 1);
    db.insert_run(&run("before", "t", Some("acc"), 999, (1, 2)))
        .unwrap();
    db.insert_run(&run("at", "t", Some("acc"), 1000, (10, 20)))
        .unwrap();
    db.insert_run(&run("after", "t", Some("acc"), 1001, (100, 200)))
        .unwrap();
    db.insert_run(&run("other", "t", Some("acc2"), 1001, (7000, 0)))
        .unwrap();
    db.insert_run(&run("anon", "t", None, 1001, (9000, 0)))
        .unwrap();

    assert_eq!(db.tokens_since("acc", 0).unwrap(), 333);
    assert_eq!(db.tokens_since("acc", 999).unwrap(), 333);
    assert_eq!(db.tokens_since("acc", 1000).unwrap(), 330);
    assert_eq!(db.tokens_since("acc", 1001).unwrap(), 300);
    assert_eq!(db.tokens_since("acc", 1002).unwrap(), 0);
    assert_eq!(db.tokens_since("acc2", 1000).unwrap(), 7000);
    assert_eq!(db.tokens_since("nobody", 0).unwrap(), 0);

    // The final numbers of a finished run replace the provisional ones.
    let finish = RunFinish {
        status: RunStatus::Succeeded,
        exit_code: Some(0),
        tokens_in: 1,
        tokens_out: 1,
        ended_at: 2000,
    };
    db.finish_run("after", &finish).unwrap();
    assert_eq!(db.tokens_since("acc", 1001).unwrap(), 2);
}

#[test]
fn accounts_upsert_keeps_cooldown_and_refreshes_profile() {
    let (_dir, db) = open();
    assert!(db.list_accounts().unwrap().is_empty());
    assert!(matches!(
        db.set_cooldown("ghost", Some(1)),
        Err(PortError::NotFound(_))
    ));

    db.upsert_account(&account("b", "/cfg/b")).unwrap();
    db.upsert_account(&account("a", "/cfg/a")).unwrap();
    db.set_cooldown("a", Some(5000)).unwrap();

    let moved = AccountProfile {
        provider: ProviderKind::Codex,
        name: "Renamed".to_string(),
        ..account("a", "/cfg/a-moved")
    };
    db.upsert_account(&moved).unwrap();

    let accounts = db.list_accounts().unwrap();
    assert_eq!(accounts.len(), 2);
    assert_eq!(accounts[0].profile, moved);
    assert_eq!(accounts[0].cooldown_until, Some(5000));
    assert_eq!(accounts[1].profile, account("b", "/cfg/b"));
    assert_eq!(accounts[1].profile.name, "name of b");
    assert_eq!(accounts[1].cooldown_until, None);

    db.set_cooldown("a", None).unwrap();
    assert_eq!(db.list_accounts().unwrap()[0].cooldown_until, None);
}

#[test]
fn every_provider_kind_round_trips() {
    let (_dir, db) = open();
    for provider in ProviderKind::ALL {
        let profile = AccountProfile {
            provider,
            ..account(provider.as_str(), "/cfg")
        };
        db.upsert_account(&profile).unwrap();
    }
    let mut stored: Vec<ProviderKind> = db
        .list_accounts()
        .unwrap()
        .into_iter()
        .map(|a| a.profile.provider)
        .collect();
    stored.sort_by_key(|p| p.as_str());
    let mut expected = ProviderKind::ALL.to_vec();
    expected.sort_by_key(|p| p.as_str());
    assert_eq!(stored, expected);
}

#[test]
fn events_get_increasing_ids_and_are_listed_newest_first() {
    let (_dir, db) = open();
    let first = FactoryEvent::new(EventKind::TaskCreated, r#"{"a":1}"#, 10).for_task("t");
    let second = FactoryEvent::new(EventKind::RunStarted, r#"{"b":"x'y"}"#, 11)
        .for_task("t")
        .for_run("run-1");
    let elsewhere = FactoryEvent::new(EventKind::GateResult, "{}", 12).for_task("u");
    let global = FactoryEvent::new(EventKind::FactoryFrozen, "{}", 13);
    // A caller-supplied id is ignored: the store assigns it.
    let third = FactoryEvent {
        id: Some(-77),
        ..FactoryEvent::new(EventKind::RunFinished, "{}", 14).for_task("t")
    };

    let id1 = db.emit(&first).unwrap();
    let id2 = db.emit(&second).unwrap();
    let id_u = db.emit(&elsewhere).unwrap();
    let id_g = db.emit(&global).unwrap();
    let id3 = db.emit(&third).unwrap();
    assert!(id1 < id2 && id2 < id_u && id_u < id_g && id_g < id3);

    let with_id = |event: &FactoryEvent, id: i64| FactoryEvent {
        id: Some(id),
        ..event.clone()
    };
    assert_eq!(
        db.events_for_task("t", 10).unwrap(),
        [
            with_id(&third, id3),
            with_id(&second, id2),
            with_id(&first, id1)
        ]
    );
    assert_eq!(
        db.events_for_task("t", 2).unwrap(),
        [with_id(&third, id3), with_id(&second, id2)]
    );
    assert!(db.events_for_task("t", 0).unwrap().is_empty());
    assert_eq!(
        db.events_for_task("u", 10).unwrap(),
        [with_id(&elsewhere, id_u)]
    );
    assert!(db.events_for_task("ghost", 10).unwrap().is_empty());
}

#[test]
fn every_event_kind_round_trips() {
    let (_dir, db) = open();
    for kind in EventKind::ALL {
        db.emit(&FactoryEvent::new(kind, "{}", 1).for_task("t"))
            .unwrap();
    }
    let mut stored: Vec<EventKind> = db
        .events_for_task("t", 100)
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    stored.reverse();
    assert_eq!(stored, EventKind::ALL);
}

fn raw_event_count(path: &Path) -> i64 {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .unwrap()
}

#[test]
fn events_table_refuses_update_and_delete_but_allows_insert() {
    let (dir, db) = open();
    db.emit(&FactoryEvent::new(EventKind::TaskCreated, "{}", 1).for_task("t"))
        .unwrap();
    drop(db);

    let path = db_path(&dir);
    let conn = rusqlite::Connection::open(&path).unwrap();
    let update = conn.execute("UPDATE events SET payload_json = 'tampered'", []);
    assert!(update.unwrap_err().to_string().contains("append-only"));
    let delete = conn.execute("DELETE FROM events", []);
    assert!(delete.unwrap_err().to_string().contains("append-only"));
    drop(conn);
    assert_eq!(raw_event_count(&path), 1);

    let db = Db::open(&path).unwrap();
    db.emit(&FactoryEvent::new(EventKind::RunStarted, "{}", 2).for_task("t"))
        .unwrap();
    let events = db.events_for_task("t", 10).unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].payload_json, "{}");
}

fn closed(fp: &str, repo: &str, task_id: &str, closed_at: i64) -> ClosedFingerprint {
    ClosedFingerprint {
        fingerprint: fp.to_string(),
        repo_root: repo.to_string(),
        task_id: task_id.to_string(),
        closed_at,
    }
}

#[test]
fn closed_fingerprints_return_the_most_recent_inside_the_window() {
    let (_dir, db) = open();
    assert_eq!(db.find_closed_since("fp", "/r", 0).unwrap(), None);

    // Recorded out of order: the newest by time must win, not the last row.
    db.record_closed(&closed("fp", "/r", "t-new", 300)).unwrap();
    db.record_closed(&closed("fp", "/r", "t-old", 100)).unwrap();
    db.record_closed(&closed("fp", "/other", "t-other", 900))
        .unwrap();
    db.record_closed(&closed("fp2", "/r", "t-fp2", 900))
        .unwrap();

    let newest = Some(closed("fp", "/r", "t-new", 300));
    assert_eq!(db.find_closed_since("fp", "/r", 0).unwrap(), newest);
    assert_eq!(db.find_closed_since("fp", "/r", 300).unwrap(), newest);
    assert_eq!(db.find_closed_since("fp", "/r", 301).unwrap(), None);
    assert_eq!(
        db.find_closed_since("fp", "/other", 0).unwrap(),
        Some(closed("fp", "/other", "t-other", 900))
    );
    assert_eq!(db.find_closed_since("nope", "/r", 0).unwrap(), None);
}

#[test]
fn find_open_task_ignores_terminal_tasks_and_other_repos() {
    let (_dir, db) = open();
    add_task(&db, "done", "/r", TaskState::Done, Some("fp"), 1);
    add_task(&db, "cancelled", "/r", TaskState::Cancelled, Some("fp"), 2);
    add_task(&db, "other-repo", "/other", TaskState::Ready, Some("fp"), 3);
    add_task(&db, "other-fp", "/r", TaskState::Ready, Some("fp2"), 4);
    add_task(&db, "no-fp", "/r", TaskState::Ready, None, 5);
    assert_eq!(db.find_open_task("fp", "/r").unwrap(), None);

    add_task(&db, "blocked", "/r", TaskState::Blocked, Some("fp"), 6);
    assert_eq!(
        db.find_open_task("fp", "/r").unwrap(),
        Some("blocked".to_string())
    );
    assert_eq!(
        db.find_open_task("fp", "/other").unwrap(),
        Some("other-repo".to_string())
    );

    db.set_state("blocked", TaskState::Cancelled, None, 7)
        .unwrap();
    assert_eq!(db.find_open_task("fp", "/r").unwrap(), None);
}

#[test]
fn meta_get_set_and_overwrite() {
    let (_dir, db) = open();
    assert_eq!(db.get_meta(META_FACTORY_FROZEN).unwrap(), None);
    db.set_meta(META_FACTORY_FROZEN, "1").unwrap();
    assert_eq!(
        db.get_meta(META_FACTORY_FROZEN).unwrap(),
        Some("1".to_string())
    );
    db.set_meta(META_FACTORY_FROZEN, "").unwrap();
    assert_eq!(
        db.get_meta(META_FACTORY_FROZEN).unwrap(),
        Some(String::new())
    );
    assert_eq!(db.get_meta("other").unwrap(), None);
}

#[test]
fn data_survives_dropping_the_last_handle_and_reopening() {
    let (dir, db) = open();
    let clone = db.clone();
    add_task(&db, "t", "/r", TaskState::Ready, None, 1);
    db.set_meta("k", "v").unwrap();
    drop(db);
    // A surviving clone keeps the writer alive.
    clone.set_meta("k2", "v2").unwrap();
    drop(clone);

    let reopened = Db::open(&db_path(&dir)).unwrap();
    assert_eq!(reopened.get_task("t").unwrap().state, TaskState::Ready);
    assert_eq!(reopened.get_meta("k").unwrap(), Some("v".to_string()));
    assert_eq!(reopened.get_meta("k2").unwrap(), Some("v2".to_string()));
}

#[test]
fn hostile_text_is_stored_as_data() {
    let (_dir, db) = open();
    let hostile = "x'); DROP TABLE meta; --";
    db.set_meta(hostile, hostile).unwrap();
    add_task(&db, hostile, hostile, TaskState::Ready, Some(hostile), 1);
    assert_eq!(db.get_meta(hostile).unwrap(), Some(hostile.to_string()));
    assert_eq!(db.list_tasks(Some(hostile), None).unwrap().len(), 1);
    assert_eq!(
        db.find_open_task(hostile, hostile).unwrap(),
        Some(hostile.to_string())
    );
}
