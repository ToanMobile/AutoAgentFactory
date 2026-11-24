//! R3-L5-1: in `Mode::Auto` a worker moves its task to `Gating` before the
//! scheduler handles its outcome. A sibling's message handled in between
//! wakes the loop, and `poll_gating` must not queue a second gate for it.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aaf_domain::event::EventKind;
use aaf_domain::ports::{EventSink, LimitHit, PortResult, TaskStore};
use aaf_domain::task::{Priority, Task, TaskState};

use super::fakes::{REPO, account, spec};
use super::world::World;
use crate::factory::ports::{AccountPool, Grant, Purpose};
use crate::factory::{Hooks, Mode, run};

const HANG: Duration = Duration::from_secs(30);

/// Holds the dropping worker thread (after its task reached `Gating`, before
/// its outcome is sent) until the release channel fires.
struct SlowLease {
    entered: Sender<()>,
    release: Mutex<Receiver<()>>,
}

impl Drop for SlowLease {
    fn drop(&mut self) {
        let _ = self.entered.send(());
        if let Ok(release) = self.release.lock() {
            let _ = release.recv_timeout(HANG);
        }
    }
}

/// Grants every job. t1's first grant carries a [`SlowLease`]; the first
/// grant for t3 releases it, so t1's outcome arrives only after the loop
/// handled t2's message and ran `poll_gating` on its way to `start_jobs`.
struct InterleavePool {
    slow: Mutex<Option<SlowLease>>,
    release_t1: Mutex<Option<Sender<()>>>,
}

impl AccountPool for InterleavePool {
    fn acquire(&self) -> PortResult<Option<Grant>> {
        Ok(Some(Grant::new(Some(account("a")), None)))
    }
    fn acquire_for(&self, task_id: &str, _purpose: Purpose) -> PortResult<Option<Grant>> {
        let lease: Option<Box<dyn std::any::Any + Send>> = match task_id {
            "t1" => self
                .slow
                .lock()
                .unwrap()
                .take()
                .map(|l| Box::new(l) as Box<dyn std::any::Any + Send>),
            "t3" => {
                if let Some(release) = self.release_t1.lock().unwrap().take() {
                    release.send(()).unwrap();
                }
                None
            }
            _ => None,
        };
        Ok(Some(Grant::new(Some(account("a")), lease)))
    }
    fn report_limit(&self, _account_id: &str, _hit: &LimitHit) -> PortResult<()> {
        Ok(())
    }
    fn profiles(&self) -> PortResult<Vec<aaf_domain::account::AccountProfile>> {
        Ok(vec![account("a")])
    }
}

#[test]
fn a_late_worker_outcome_does_not_gate_its_task_twice_in_auto_mode() {
    let mut world = World::new();
    world.max_parallel = 2;
    world.add("t1");
    world.add("t2");
    let (entered_tx, entered) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (go, go_rx) = mpsc::channel();
    world
        .runner
        .0
        .lock()
        .unwrap()
        .hold
        .insert("t2".to_string(), go_rx);
    let mut deps = world.deps();
    deps.accounts = Arc::new(InterleavePool {
        slow: Mutex::new(Some(SlowLease {
            entered: entered_tx,
            release: Mutex::new(release_rx),
        })),
        release_t1: Mutex::new(Some(release_tx)),
    });
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };
    let summary = std::thread::scope(|scope| {
        let handle = scope.spawn(|| run(&deps, Mode::Auto, &hooks).unwrap());
        // t1's worker is past `move_to(Gating)`, its outcome not yet sent.
        entered.recv_timeout(HANG).unwrap();
        assert_eq!(world.state("t1"), TaskState::Gating);
        // t3 becomes a candidate only now, so its grant is asked for after
        // t2's message is handled.
        let mut t3 = Task::new_root("t3", REPO, "task t3", spec(), Priority::P2, 1);
        t3.state = TaskState::Ready;
        world.db.insert_task(&t3).unwrap();
        go.send(()).unwrap();
        handle.join().unwrap()
    });

    let t1_rounds = world
        .gate
        .runs()
        .iter()
        .filter(|ids| ids.iter().any(|id| id == "t1"))
        .count();
    let t1_results = world
        .db
        .events_for_task("t1", 1_000)
        .unwrap()
        .iter()
        .filter(|e| e.kind == EventKind::GateResult)
        .count();
    assert_eq!(t1_rounds, 1, "t1 gated more than once: {summary:?}");
    assert_eq!(t1_results, 1, "{summary:?}");
    assert_eq!(world.state("t1"), TaskState::Review, "{summary:?}");
    assert!(!summary.blocked.contains(&"t1".to_string()), "{summary:?}");
    assert!(summary.errors.is_empty(), "{summary:?}");
}
