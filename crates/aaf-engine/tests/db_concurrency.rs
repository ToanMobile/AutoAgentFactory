#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Concurrent writers: threads sharing one handle, and independent handles
//! on one file standing in for separate processes.

use std::path::Path;
use std::sync::{Arc, Barrier};
use std::thread;

use aaf_domain::budget::RootBudget;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{BudgetStore, EventSink, KvStore, TaskStore};
use aaf_domain::task::{ImmutableSpec, Priority, Task};
use aaf_engine::db::Db;

const THREADS: usize = 8;
const PER_THREAD: usize = 50;

fn new_task(id: &str) -> Task {
    let spec = ImmutableSpec {
        description: "d".to_string(),
        acceptance: vec![],
    };
    Task::new_root(id, "/r", "t", spec, Priority::P2, 1)
}

/// Runs `work(thread_index)` on `THREADS` threads released together.
fn run_together<F>(work: F)
where
    F: Fn(usize) + Send + Sync + 'static,
{
    let work = Arc::new(work);
    let barrier = Arc::new(Barrier::new(THREADS));
    let handles: Vec<_> = (0..THREADS)
        .map(|index| {
            let work = Arc::clone(&work);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                work(index);
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
}

#[test]
fn db_handle_is_clone_send_and_sync() {
    fn assert_handle<T: Clone + Send + Sync + 'static>() {}
    assert_handle::<Db>();
}

#[test]
fn eight_threads_on_one_handle_lose_no_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("aaf.db")).unwrap();

    let writer = db.clone();
    run_together(move |thread_index| {
        for n in 0..PER_THREAD {
            let id = format!("t{thread_index}-{n}");
            writer.insert_task(&new_task(&id)).unwrap();
            let event = FactoryEvent::new(EventKind::TaskCreated, "{}", 1).for_task("shared");
            writer.emit(&event).unwrap();
        }
    });

    let total = THREADS * PER_THREAD;
    assert_eq!(db.list_tasks(None, None).unwrap().len(), total);
    let events = db.events_for_task("shared", u32::MAX).unwrap();
    assert_eq!(events.len(), total);
    let mut ids: Vec<i64> = events.iter().filter_map(|e| e.id).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), total, "event ids must be unique");
}

#[test]
fn add_usage_is_atomic_across_threads_and_handles() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aaf.db");
    let first = Db::open(&path).unwrap();
    let second = Db::open(&path).unwrap();
    first
        .put_budget(&RootBudget {
            root_id: "r".to_string(),
            tokens_used: 0,
            token_limit: 1,
            wall_secs_used: 0,
            wall_secs_limit: 1,
            cycles_used: 0,
            cycles_limit: 1,
            frozen: false,
        })
        .unwrap();

    let handles = [first.clone(), second];
    run_together(move |thread_index| {
        // Half of the threads go through each "process".
        let db = &handles[thread_index % 2];
        for _ in 0..PER_THREAD {
            db.add_usage("r", 3, 2, 1).unwrap();
        }
    });

    let calls = (THREADS * PER_THREAD) as u64;
    let budget = first.get_budget("r").unwrap();
    assert_eq!(budget.tokens_used, 3 * calls);
    assert_eq!(budget.wall_secs_used, 2 * calls);
    assert_eq!(u64::from(budget.cycles_used), calls);
}

#[test]
fn two_independent_opens_of_one_file_both_write_without_busy_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aaf.db");
    let first = Db::open(&path).unwrap();
    let second = Db::open(&path).unwrap();

    let handles = [first.clone(), second.clone()];
    run_together(move |thread_index| {
        let process = thread_index % 2;
        let db = &handles[process];
        for n in 0..PER_THREAD {
            let id = format!("p{process}-t{thread_index}-{n}");
            // Any SQLITE_BUSY would surface here as PortError::Io.
            db.insert_task(&new_task(&id)).unwrap();
            db.set_meta(&format!("k-{id}"), "v").unwrap();
        }
    });

    let total = THREADS * PER_THREAD;
    // Each handle sees the other's rows.
    assert_eq!(first.list_tasks(None, None).unwrap().len(), total);
    assert_eq!(second.list_tasks(None, None).unwrap().len(), total);
    assert_eq!(second.get_meta("k-p0-t0-0").unwrap(), Some("v".to_string()));
    assert_eq!(first.get_meta("k-p1-t1-0").unwrap(), Some("v".to_string()));
}

fn open_and_write(path: &Path, key: &str) {
    let db = Db::open(path).unwrap();
    db.set_meta(key, "v").unwrap();
}

#[test]
fn racing_first_opens_of_a_fresh_file_all_succeed() {
    let dir = tempfile::tempdir().unwrap();
    // Several fresh files: the race between the first openers (switch to
    // WAL, first migration) is narrow, so one round rarely hits it.
    for round in 0..20 {
        let path = dir.path().join(format!("aaf-{round}.db"));

        let racing_path = path.clone();
        run_together(move |thread_index| {
            open_and_write(&racing_path, &format!("k{thread_index}"));
        });

        let db = Db::open(&path).unwrap();
        for thread_index in 0..THREADS {
            let key = format!("k{thread_index}");
            assert_eq!(db.get_meta(&key).unwrap(), Some("v".to_string()));
        }
    }
}
