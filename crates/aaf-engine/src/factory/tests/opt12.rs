//! Focused unit tests for Round 3A Fixer 4: defensive human unblock note,
//! `AuditScope::Large` per-job batching in `judge_round`, and `PROPOSER_EXECUTOR`
//! recording + `RunHistory` exclusion.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use aaf_domain::account::ProviderKind;
use aaf_domain::blocked::BlockedReason;
use aaf_domain::budget::RootBudget;
use aaf_domain::config::AuditScope;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{
    AccountStore, AgentOutcome, AgentRequest, AgentRunner, BudgetStore, ClosedFingerprint,
    EventSink, FingerprintStore, KvStore, LimitHit, PortResult, RunStore, TaskStore,
};
use aaf_domain::run::{RunFinish, RunRecord, RunStatus};
use aaf_domain::task::{Priority, Task, TaskState};

use super::fakes::{REPO, account, spec};
use super::world::World;
use crate::accounts::RunHistory;
use crate::db::Db;
use crate::factory::finish::{GateStats, Resolution, judge_round};
use crate::factory::ports::GitOps;
use crate::factory::worker::GateJob;
use crate::factory::{Hooks, Mode, Store, run};
use crate::unblocker::PROPOSER_EXECUTOR;

#[test]
fn defensive_unblocked_note_resets_ladder_when_only_event_was_emitted() {
    let world = World::new();
    world.add("t1");
    world.gate.state.lock().unwrap().red.insert("t1".into());

    assert_eq!(world.once().requeued, vec!["t1"]);
    // Simulate a caller emitting a human unblock event without appending an "unblocked:" note.
    let payload = serde_json::json!({ "to": "ready", "why": "aaf ui unblock" }).to_string();
    world
        .db
        .emit(&FactoryEvent::new(EventKind::TaskStateChanged, payload, 1_005).for_task("t1"))
        .unwrap();

    // Second run must append the defensive note once and reset the ladder to T3.
    let s2 = world.once();
    assert_eq!(s2.requeued, vec!["t1"]);
    let notes = world.task("t1").context_notes;
    assert_eq!(
        notes
            .iter()
            .filter(|n| *n == "unblocked: human unblock")
            .count(),
        1,
        "{notes:?}"
    );
}

#[test]
fn large_audit_scope_batches_small_diffs_and_isolates_large_diff() {
    let mut world = World::new();
    world.repo.cross_audit.audit_scope = AuditScope::Large;
    let jobs: Vec<GateJob> = ["s1", "s2", "big"]
        .into_iter()
        .map(|id| {
            let mut task = Task::new_root(id, REPO, id, spec(), Priority::P1, 1);
            task.state = TaskState::Gating;
            world.db.insert_task(&task).unwrap();
            let wt = world.git.task_worktree(id).unwrap();
            GateJob {
                task_id: id.into(),
                worktree: wt,
                audited: false,
                cached_receipt: None,
            }
        })
        .collect();
    // Make `big` exceed the >=3 changed-files Large threshold in FakeGit.
    let big_wt = world.git.task_worktree("big").unwrap();
    world.git.0.lock().unwrap().diffs.insert(
        big_wt.path,
        "diff --git a/a.rs b/a.rs\n+1\ndiff --git a/b.rs b/b.rs\n+2\ndiff --git a/c.rs b/c.rs\n+3\n"
            .into(),
    );

    let stats = GateStats::default();
    let answers = Mutex::new(Vec::new());
    judge_round(&world.deps(), jobs, &stats, &|job, res| {
        let label = match res {
            Ok(Resolution::Settled(st)) => st.as_str().to_string(),
            Ok(Resolution::NeedsPostAudit { .. }) => "needs-post-audit".to_string(),
            other => format!("unexpected: {}", other.is_ok()),
        };
        answers
            .lock()
            .unwrap()
            .push(format!("{}:{label}", job.task_id));
    });

    // (s1, s2) batched into 1 integration gate run -> review; big gated solo -> needs-post-audit.
    assert_eq!(stats.runs.load(Ordering::Relaxed), 2);
    let runs = world.gate.runs();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0], vec!["s1", "s2"]);
    assert_eq!(runs[1], vec!["big"]);
    let mut got = answers.into_inner().unwrap();
    got.sort();
    assert_eq!(got, vec!["big:needs-post-audit", "s1:review", "s2:review"]);
}

#[test]
fn proposer_run_is_recorded_and_excluded_from_run_history() {
    let world = World::new();
    world.add("t1");
    world.gate.state.lock().unwrap().red.insert("t1".into());

    let _ = world.once();
    let _ = world.once();
    let runs = world.db.runs_for_task("t1").unwrap();
    let prop = runs
        .iter()
        .find(|r| r.executor == PROPOSER_EXECUTOR)
        .expect("proposer run must be recorded in runs table");
    assert_eq!(prop.status, RunStatus::Succeeded);
    assert!(world.db.get_budget("t1").is_ok());
    let events = world.db.events_for_task("t1", 50).unwrap();
    assert!(events.iter().any(|e| {
        e.kind == EventKind::RunStarted && e.payload_json.contains(PROPOSER_EXECUTOR)
    }));

    // A task whose only run is PROPOSER_EXECUTOR must be ignored by RunHistory.
    world.db.upsert_account(&account("a")).unwrap();
    world.add("t-only-prop");
    world
        .db
        .insert_run(&RunRecord {
            id: "r-prop".into(),
            task_id: "t-only-prop".into(),
            account_id: Some("a".into()),
            executor: PROPOSER_EXECUTOR.into(),
            worktree_path: String::new(),
            branch: String::new(),
            status: RunStatus::Succeeded,
            exit_code: Some(0),
            tokens_in: 1,
            tokens_out: 1,
            started_at: 2_000,
            ended_at: Some(2_005),
        })
        .unwrap();
    assert_eq!(
        world
            .db
            .last_account_of_task("t-only-prop", ProviderKind::ClaudeCode)
            .unwrap(),
        None
    );
}

struct CountingStore(Db, AtomicUsize);
macro_rules! fwd {
    ($($f:ident($($a:ident : $t:ty),*) -> $r:ty);* $(;)?) => {
        $(fn $f(&self, $($a: $t),*) -> $r { self.0.$f($($a),*) })*
    };
}
impl TaskStore for CountingStore {
    fn list_tasks(&self, r: Option<&str>, s: Option<TaskState>) -> PortResult<Vec<Task>> {
        if s.is_none() {
            self.1.fetch_add(1, Ordering::SeqCst);
        }
        self.0.list_tasks(r, s)
    }
    fwd!(
        insert_task(t: &Task) -> PortResult<()>; get_task(id: &str) -> PortResult<Task>;
        set_state(id: &str, s: TaskState, b: Option<&BlockedReason>, now: i64) -> PortResult<()>;
        append_context_note(id: &str, n: &str, now: i64) -> PortResult<()>;
        increment_unblock_cycles(id: &str, now: i64) -> PortResult<u32>;
        add_dep(a: &str, b: &str) -> PortResult<()>; remove_dep(a: &str, b: &str) -> PortResult<bool>;
        deps_of(id: &str) -> PortResult<Vec<String>>; all_deps() -> PortResult<Vec<(String, String)>>;
        children_of(id: &str) -> PortResult<Vec<Task>>;
    );
}
impl BudgetStore for CountingStore {
    fwd!(
        put_budget(b: &RootBudget) -> PortResult<()>; get_budget(id: &str) -> PortResult<RootBudget>;
        add_usage(id: &str, t: u64, w: u64, c: u32) -> PortResult<RootBudget>;
        set_frozen(id: &str, f: bool) -> PortResult<()>;
    );
}
impl RunStore for CountingStore {
    fwd!(
        insert_run(r: &RunRecord) -> PortResult<()>; finish_run(id: &str, f: &RunFinish) -> PortResult<()>;
        runs_for_task(id: &str) -> PortResult<Vec<RunRecord>>;
        tokens_since(id: &str, s: i64) -> PortResult<u64>;
    );
}
impl EventSink for CountingStore {
    fwd!(
        emit(ev: &FactoryEvent) -> PortResult<i64>;
        events_for_task(id: &str, l: u32) -> PortResult<Vec<FactoryEvent>>;
    );
}
impl KvStore for CountingStore {
    fwd!(
        get_meta(k: &str) -> PortResult<Option<String>>;
        set_meta(k: &str, v: &str) -> PortResult<()>;
    );
}
impl FingerprintStore for CountingStore {
    fwd!(
        record_closed(c: &ClosedFingerprint) -> PortResult<()>;
        find_closed_since(fp: &str, r: &str, s: i64) -> PortResult<Option<ClosedFingerprint>>;
        find_open_task(fp: &str, r: &str) -> PortResult<Option<String>>;
    );
}
impl Store for CountingStore {}

fn ok_outcome(text: &str) -> AgentOutcome {
    AgentOutcome {
        exit_code: Some(0),
        timed_out: false,
        final_text: text.into(),
        tokens_in: 10,
        tokens_out: 5,
        limit_hit: None,
        infra: None,
        log_tail: String::new(),
    }
}

#[test]
fn opt12_5_single_snapshot_per_tick_and_idle_slot_wakeup() {
    let mut world = World::with_accounts(&[]);
    world.max_parallel = 4;
    for i in 1..=4 {
        world.add(&format!("p{i}"));
    }
    let store = Arc::new(CountingStore(world.db.clone(), AtomicUsize::new(0)));
    let (sc, snap) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    struct BarrierRunner(Arc<AtomicUsize>, Arc<AtomicUsize>, Arc<CountingStore>);
    impl AgentRunner for BarrierRunner {
        fn run(&self, _: &AgentRequest) -> PortResult<AgentOutcome> {
            let n = self.0.fetch_add(1, Ordering::SeqCst) + 1;
            let t0 = Instant::now();
            while self.0.load(Ordering::SeqCst) < 4 && t0.elapsed() < Duration::from_secs(2) {
                std::thread::sleep(Duration::from_millis(5));
            }
            if n == 4 {
                self.1
                    .store(self.2.1.load(Ordering::SeqCst), Ordering::SeqCst);
            }
            Ok(ok_outcome("ok"))
        }
    }
    let mut deps = world.deps();
    deps.store = store.clone();
    deps.runner = Arc::new(BarrierRunner(sc, snap.clone(), store));
    let hooks = Hooks {
        idle: &|| false,
        reap: &|_| Ok(()),
    };
    assert_eq!(run(&deps, Mode::Once, &hooks).unwrap().review.len(), 4);
    assert_eq!(snap.load(Ordering::SeqCst), 2);

    // Mode::Auto idle-slot wakeup while sibling worker holds a latch.
    let mut w2 = World::with_accounts(&[]);
    w2.max_parallel = 2;
    w2.add("t-slow");
    w2.add("t-wait");
    let pair = Arc::new((Mutex::new(false), Condvar::new()));
    struct LatchRunner(
        Arc<(Mutex<bool>, Condvar)>,
        Arc<super::fakes::FakeClock>,
        AtomicUsize,
    );
    impl AgentRunner for LatchRunner {
        fn run(&self, req: &AgentRequest) -> PortResult<AgentOutcome> {
            if req.prompt.contains("task t-slow") {
                let done = self.0.0.lock().unwrap();
                let _ = self
                    .0
                    .1
                    .wait_timeout_while(done, Duration::from_secs(4), |d| !*d);
                return Ok(ok_outcome("slow"));
            }
            if self.2.fetch_add(1, Ordering::SeqCst) == 0 {
                let mut out = ok_outcome("");
                out.exit_code = Some(1);
                out.limit_hit = Some(LimitHit {
                    reset_at: Some(1_001),
                    raw: "429".into(),
                });
                let c = self.1.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(150));
                    c.0.fetch_add(5, Ordering::SeqCst);
                });
                return Ok(out);
            }
            *self.0.0.lock().unwrap() = true;
            self.0.1.notify_all();
            Ok(ok_outcome("wait"))
        }
    }
    let mut d2 = w2.deps();
    d2.runner = Arc::new(LatchRunner(
        pair.clone(),
        w2.clock.clone(),
        AtomicUsize::new(0),
    ));
    assert_eq!(run(&d2, Mode::Auto, &hooks).unwrap().review.len(), 2);
    assert!(
        *pair.0.lock().unwrap(),
        "t-wait must wake while t-slow runs"
    );
}
