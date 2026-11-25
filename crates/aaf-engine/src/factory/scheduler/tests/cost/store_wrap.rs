//! Counting wrapper around `Db` used by `tests/cost.rs` to assert that
//! disabling `cost_guard` never invokes `Store::cost_window_usage`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use aaf_domain::blocked::BlockedReason;
use aaf_domain::budget::RootBudget;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{
    BudgetStore, ClosedFingerprint, EventSink, FingerprintStore, KvStore, PortResult, RunStore,
    TaskStore,
};
use aaf_domain::run::{RunFinish, RunRecord};
use aaf_domain::task::{Task, TaskState};

use crate::db::Db;
use crate::factory::Store;

pub(super) struct CountingStore {
    pub(super) inner: Db,
    pub(super) cost_queries: Arc<AtomicU32>,
}

impl TaskStore for CountingStore {
    fn insert_task(&self, task: &Task) -> PortResult<()> {
        self.inner.insert_task(task)
    }

    fn get_task(&self, id: &str) -> PortResult<Task> {
        self.inner.get_task(id)
    }

    fn list_tasks(
        &self,
        repo_root: Option<&str>,
        state: Option<TaskState>,
    ) -> PortResult<Vec<Task>> {
        self.inner.list_tasks(repo_root, state)
    }

    fn set_state(
        &self,
        id: &str,
        state: TaskState,
        blocked: Option<&BlockedReason>,
        now: i64,
    ) -> PortResult<()> {
        self.inner.set_state(id, state, blocked, now)
    }

    fn append_context_note(&self, id: &str, note: &str, now: i64) -> PortResult<()> {
        self.inner.append_context_note(id, note, now)
    }

    fn increment_unblock_cycles(&self, id: &str, now: i64) -> PortResult<u32> {
        self.inner.increment_unblock_cycles(id, now)
    }

    fn add_dep(&self, task_id: &str, blocked_by: &str) -> PortResult<()> {
        self.inner.add_dep(task_id, blocked_by)
    }

    fn remove_dep(&self, task_id: &str, blocked_by: &str) -> PortResult<bool> {
        self.inner.remove_dep(task_id, blocked_by)
    }

    fn deps_of(&self, task_id: &str) -> PortResult<Vec<String>> {
        self.inner.deps_of(task_id)
    }

    fn all_deps(&self) -> PortResult<Vec<(String, String)>> {
        self.inner.all_deps()
    }

    fn children_of(&self, parent_id: &str) -> PortResult<Vec<Task>> {
        self.inner.children_of(parent_id)
    }

    fn set_retry_schedule(
        &self,
        id: &str,
        retry_after: Option<i64>,
        auto_recover_count: u32,
        now: i64,
    ) -> PortResult<()> {
        self.inner
            .set_retry_schedule(id, retry_after, auto_recover_count, now)
    }

    fn wake_due_blocked_tasks(&self, repo_root: &str, now: i64) -> PortResult<Vec<String>> {
        self.inner.wake_due_blocked_tasks(repo_root, now)
    }
}

impl BudgetStore for CountingStore {
    fn get_budget(&self, root_id: &str) -> PortResult<RootBudget> {
        self.inner.get_budget(root_id)
    }

    fn put_budget(&self, budget: &RootBudget) -> PortResult<()> {
        self.inner.put_budget(budget)
    }

    fn add_usage(
        &self,
        root_id: &str,
        tokens: u64,
        wall_secs: u64,
        cycles: u32,
    ) -> PortResult<RootBudget> {
        self.inner.add_usage(root_id, tokens, wall_secs, cycles)
    }

    fn set_frozen(&self, root_id: &str, frozen: bool) -> PortResult<()> {
        self.inner.set_frozen(root_id, frozen)
    }
}

impl RunStore for CountingStore {
    fn insert_run(&self, run: &RunRecord) -> PortResult<()> {
        self.inner.insert_run(run)
    }

    fn finish_run(&self, id: &str, finish: &RunFinish) -> PortResult<()> {
        self.inner.finish_run(id, finish)
    }

    fn runs_for_task(&self, task_id: &str) -> PortResult<Vec<RunRecord>> {
        self.inner.runs_for_task(task_id)
    }

    fn tokens_since(&self, account_id: &str, since: i64) -> PortResult<u64> {
        self.inner.tokens_since(account_id, since)
    }
}

impl EventSink for CountingStore {
    fn emit(&self, event: &FactoryEvent) -> PortResult<i64> {
        self.inner.emit(event)
    }

    fn events_for_task(&self, task_id: &str, limit: u32) -> PortResult<Vec<FactoryEvent>> {
        self.inner.events_for_task(task_id, limit)
    }

    fn events_for_task_by_kinds(
        &self,
        task_id: &str,
        kinds: &[EventKind],
        limit: u32,
    ) -> PortResult<Vec<FactoryEvent>> {
        self.inner.events_for_task_by_kinds(task_id, kinds, limit)
    }
}

impl KvStore for CountingStore {
    fn get_meta(&self, key: &str) -> PortResult<Option<String>> {
        self.inner.get_meta(key)
    }

    fn set_meta(&self, key: &str, value: &str) -> PortResult<()> {
        self.inner.set_meta(key, value)
    }
}

impl FingerprintStore for CountingStore {
    fn record_closed(&self, closed: &ClosedFingerprint) -> PortResult<()> {
        self.inner.record_closed(closed)
    }

    fn find_closed_since(
        &self,
        fingerprint: &str,
        repo_root: &str,
        since: i64,
    ) -> PortResult<Option<ClosedFingerprint>> {
        self.inner.find_closed_since(fingerprint, repo_root, since)
    }

    fn find_open_task(&self, fingerprint: &str, repo_root: &str) -> PortResult<Option<String>> {
        self.inner.find_open_task(fingerprint, repo_root)
    }
}

impl Store for CountingStore {
    fn cost_window_usage(&self, since: i64) -> PortResult<(u64, Option<i64>)> {
        self.cost_queries.fetch_add(1, Ordering::SeqCst);
        self.inner.cost_window_usage(since)
    }
}
