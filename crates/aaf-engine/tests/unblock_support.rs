//! In-memory fakes shared by the `unblock_*` tests (included with `#[path]`).
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use aaf_domain::blocked::{BlockedReason, SubtaskPlan};
use aaf_domain::budget::RootBudget;
use aaf_domain::config::UnblockerConfig;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::gate::{
    CommandResult, GateMode, GateStatus, HeadLockReport, LockStatus, RECEIPT_SCHEMA_VERSION,
    Receipt, StoreComplianceReport,
};
use aaf_domain::ports::{
    AgentOutcome, AgentRequest, AgentRunner, BudgetStore, Clock, EventSink, KvStore, LimitHit,
    PortError, PortResult, SandboxPolicy, TaskStore,
};
use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};
use aaf_engine::unblocker::{FailureSignal, Ladder, SubtaskProposer};

pub struct FixedClock(pub i64);

impl Clock for FixedClock {
    fn now(&self) -> i64 {
        self.0
    }
}

#[derive(Default)]
pub struct MemTasks(pub Mutex<BTreeMap<String, Task>>);

impl MemTasks {
    fn with<T>(&self, id: &str, f: impl FnOnce(&mut Task) -> T) -> PortResult<T> {
        let mut map = self.0.lock().unwrap();
        map.get_mut(id)
            .map(f)
            .ok_or_else(|| PortError::NotFound(id.to_string()))
    }
}

impl TaskStore for MemTasks {
    fn insert_task(&self, task: &Task) -> PortResult<()> {
        self.0.lock().unwrap().insert(task.id.clone(), task.clone());
        Ok(())
    }
    fn get_task(&self, id: &str) -> PortResult<Task> {
        self.with(id, |t| t.clone())
    }
    fn list_tasks(&self, _repo: Option<&str>, state: Option<TaskState>) -> PortResult<Vec<Task>> {
        let map = self.0.lock().unwrap();
        Ok(map
            .values()
            .filter(|t| state.is_none_or(|s| t.state == s))
            .cloned()
            .collect())
    }
    fn set_state(
        &self,
        id: &str,
        state: TaskState,
        blocked: Option<&BlockedReason>,
        now: i64,
    ) -> PortResult<()> {
        self.with(id, |t| {
            t.state = state;
            t.blocked_reason = blocked.cloned();
            t.updated_at = now;
        })
    }
    fn append_context_note(&self, id: &str, note: &str, now: i64) -> PortResult<()> {
        self.with(id, |t| {
            t.context_notes.push(note.to_string());
            t.updated_at = now;
        })
    }
    fn increment_unblock_cycles(&self, id: &str, _now: i64) -> PortResult<u32> {
        self.with(id, |t| {
            t.unblock_cycles += 1;
            t.unblock_cycles
        })
    }
    fn add_dep(&self, _task_id: &str, _blocked_by: &str) -> PortResult<()> {
        Ok(())
    }
    fn remove_dep(&self, _task_id: &str, _blocked_by: &str) -> PortResult<bool> {
        Ok(false)
    }
    fn deps_of(&self, _task_id: &str) -> PortResult<Vec<String>> {
        Ok(Vec::new())
    }
    fn all_deps(&self) -> PortResult<Vec<(String, String)>> {
        Ok(Vec::new())
    }
    fn children_of(&self, parent_id: &str) -> PortResult<Vec<Task>> {
        let map = self.0.lock().unwrap();
        Ok(map
            .values()
            .filter(|t| t.parent_id.as_deref() == Some(parent_id))
            .cloned()
            .collect())
    }
}

#[derive(Default)]
pub struct MemBudgets(pub Mutex<BTreeMap<String, RootBudget>>);

impl BudgetStore for MemBudgets {
    fn put_budget(&self, budget: &RootBudget) -> PortResult<()> {
        let mut map = self.0.lock().unwrap();
        map.insert(budget.root_id.clone(), budget.clone());
        Ok(())
    }
    fn get_budget(&self, root_id: &str) -> PortResult<RootBudget> {
        let map = self.0.lock().unwrap();
        map.get(root_id)
            .cloned()
            .ok_or_else(|| PortError::NotFound(root_id.to_string()))
    }
    fn add_usage(
        &self,
        root_id: &str,
        tokens: u64,
        wall: u64,
        cycles: u32,
    ) -> PortResult<RootBudget> {
        let mut map = self.0.lock().unwrap();
        let budget = map
            .get_mut(root_id)
            .ok_or_else(|| PortError::NotFound(root_id.to_string()))?;
        budget.add_usage(tokens, wall, cycles);
        Ok(budget.clone())
    }
    fn set_frozen(&self, root_id: &str, frozen: bool) -> PortResult<()> {
        let mut map = self.0.lock().unwrap();
        let budget = map
            .get_mut(root_id)
            .ok_or_else(|| PortError::NotFound(root_id.to_string()))?;
        budget.frozen = frozen;
        Ok(())
    }
}

#[derive(Default)]
pub struct MemKv(pub Mutex<BTreeMap<String, String>>);

impl KvStore for MemKv {
    fn get_meta(&self, key: &str) -> PortResult<Option<String>> {
        Ok(self.0.lock().unwrap().get(key).cloned())
    }
    fn set_meta(&self, key: &str, value: &str) -> PortResult<()> {
        let mut map = self.0.lock().unwrap();
        map.insert(key.to_string(), value.to_string());
        Ok(())
    }
}

#[derive(Default)]
pub struct MemEvents(pub Mutex<Vec<FactoryEvent>>);

impl MemEvents {
    pub fn count(&self, kind: EventKind) -> usize {
        let events = self.0.lock().unwrap();
        events.iter().filter(|e| e.kind == kind).count()
    }
    pub fn last(&self, kind: EventKind) -> FactoryEvent {
        let events = self.0.lock().unwrap();
        events
            .iter()
            .rev()
            .find(|e| e.kind == kind)
            .cloned()
            .unwrap()
    }
}

impl EventSink for MemEvents {
    fn emit(&self, event: &FactoryEvent) -> PortResult<i64> {
        let mut events = self.0.lock().unwrap();
        events.push(event.clone());
        Ok(events.len() as i64)
    }
    fn events_for_task(&self, task_id: &str, limit: u32) -> PortResult<Vec<FactoryEvent>> {
        let events = self.0.lock().unwrap();
        Ok(events
            .iter()
            .rev()
            .filter(|e| e.task_id.as_deref() == Some(task_id))
            .take(limit as usize)
            .cloned()
            .collect())
    }
}

/// Returns canned plans and counts how often it was asked.
pub struct FakeProposer {
    pub plans: PortResult<Vec<SubtaskPlan>>,
    pub calls: AtomicU32,
}

impl FakeProposer {
    pub fn new(plans: PortResult<Vec<SubtaskPlan>>) -> FakeProposer {
        FakeProposer {
            plans,
            calls: AtomicU32::new(0),
        }
    }
    pub fn calls(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }
}

impl SubtaskProposer for FakeProposer {
    fn propose(&self, _t: &Task, _r: &BlockedReason, _d: &str) -> PortResult<Vec<SubtaskPlan>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.plans.clone()
    }
}

/// Returns a canned outcome and keeps the prompts it was given.
pub struct FakeRunner {
    pub outcome: PortResult<AgentOutcome>,
    pub prompts: Mutex<Vec<String>>,
}

impl FakeRunner {
    pub fn new(outcome: PortResult<AgentOutcome>) -> FakeRunner {
        FakeRunner {
            outcome,
            prompts: Mutex::new(Vec::new()),
        }
    }
}

impl AgentRunner for FakeRunner {
    fn run(&self, request: &AgentRequest) -> PortResult<AgentOutcome> {
        self.prompts.lock().unwrap().push(request.prompt.clone());
        self.outcome.clone()
    }
}

pub const ROOT: &str = "root-1";
pub const CHILD: &str = "child-1";

/// One root task, one depth-1 child and a fresh root budget.
pub struct World {
    pub tasks: MemTasks,
    pub budgets: MemBudgets,
    pub kv: MemKv,
    pub events: MemEvents,
    pub clock: FixedClock,
    pub cfg: UnblockerConfig,
}

impl World {
    pub fn new(cfg: UnblockerConfig) -> World {
        let world = World {
            tasks: MemTasks::default(),
            budgets: MemBudgets::default(),
            kv: MemKv::default(),
            events: MemEvents::default(),
            clock: FixedClock(1_000),
            cfg,
        };
        let root = Task::new_root(ROOT, "/repo", "Fix login", spec(), Priority::P1, 1);
        let child = Task::new_subtask(CHILD, &root, "Part one", spec(), 1).unwrap();
        world.tasks.insert_task(&root).unwrap();
        world.tasks.insert_task(&child).unwrap();
        world
            .budgets
            .put_budget(&RootBudget::new(ROOT, &world.cfg))
            .unwrap();
        world
    }

    pub fn ladder(&self) -> Ladder<'_> {
        Ladder::new(
            &self.tasks,
            &self.budgets,
            &self.kv,
            &self.events,
            &self.clock,
            &self.cfg,
        )
    }

    pub fn task(&self, id: &str) -> Task {
        self.tasks.get_task(id).unwrap()
    }
}

pub fn spec() -> ImmutableSpec {
    ImmutableSpec {
        description: "Login must reject expired tokens.\nKeep the API stable.".to_string(),
        acceptance: vec!["expired token returns 401".to_string()],
    }
}

pub fn plan(title: &str) -> SubtaskPlan {
    SubtaskPlan {
        title: title.to_string(),
        description: format!("Do {title}"),
        acceptance: vec![format!("{title} works")],
    }
}

pub fn outcome(exit_code: i32, log_tail: &str) -> AgentOutcome {
    AgentOutcome {
        exit_code: Some(exit_code),
        timed_out: false,
        final_text: String::new(),
        tokens_in: 0,
        tokens_out: 0,
        limit_hit: None,
        infra: None,
        log_tail: log_tail.to_string(),
    }
}

pub fn limit_outcome(reset_at: Option<i64>) -> AgentOutcome {
    AgentOutcome {
        limit_hit: Some(LimitHit {
            reset_at,
            raw: "usage limit reached".to_string(),
        }),
        ..outcome(1, "usage limit reached")
    }
}

pub fn receipt(status: GateStatus, reasons: &[&str], test_log: &str) -> Receipt {
    Receipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        repo_root: "/repo".to_string(),
        tree_fp: "fp".to_string(),
        head_sha: None,
        base_sha: None,
        mode: GateMode::Scoped,
        status,
        reasons: reasons.iter().map(|r| (*r).to_string()).collect(),
        config_hash: "cfg".to_string(),
        commands: vec![CommandResult {
            name: "test_scoped".to_string(),
            command: "cargo test".to_string(),
            exit_code: Some(if status == GateStatus::Clear { 0 } else { 101 }),
            timed_out: false,
            duration_ms: 10,
            log_tail: test_log.to_string(),
        }],
        static_findings: Vec::new(),
        suppressions: 0,
        head_lock: HeadLockReport {
            status: LockStatus::Clear,
            lock_fp: String::new(),
            locked_changes: Vec::new(),
            approval: None,
        },
        store_compliance: StoreComplianceReport::not_applicable(),
        created_at: 1,
    }
}

/// A plain agent failure with no more specific cause.
pub fn stuck_signal(log_tail: &str) -> FailureSignal {
    FailureSignal {
        outcome: Some(outcome(1, log_tail)),
        ..FailureSignal::empty()
    }
}

pub fn request() -> AgentRequest {
    AgentRequest {
        run_id: "run-1".to_string(),
        prompt: String::new(),
        cwd: "/repo".to_string(),
        account: None,
        timeout_secs: 60,
        sandbox: SandboxPolicy::default(),
        json_schema: None,
    }
}
