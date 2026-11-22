//! The factory loop (docs/PLAN.md C.9 and C.11): Ready task -> agent run in
//! its own worktree -> batched gate -> cross-audit, PR or merge queue, with
//! the unblocker ladder on every failure.
//!
//! Everything is driven through port traits ([`Store`], `AgentRunner`,
//! `Clock`) and the small traits of [`ports`], so the whole loop runs against
//! fakes. [`adapters`] and [`RealGit`] are the production implementations.
//!
//! Threads: the scheduler (caller's thread) owns all bookkeeping; one thread
//! per job that needs an account; one gate thread for the repository. An
//! agent worker ends as soon as its task is handed to the gate queue, so
//! waiting for the gate never holds a worker slot or an account lease.
//! ponytail: thread per run, upgrade to a pool when > 64 runs are parallel.

pub mod adapters;
pub mod audit_backend;
pub mod batch;
mod commit_lock;
mod finish;
mod git_real;
pub mod ports;
mod post_audit;
pub mod prompt;
pub mod schedule_order;
mod scheduler;
mod select_pool;
pub mod snapshots;
mod sweep;
mod unblock;
mod worker;

#[cfg(test)]
mod tests;

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use aaf_domain::blocked::BlockedReason;
use aaf_domain::config::RepoConfig;
use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::{
    AgentRunner, BudgetStore, Clock, EventSink, FingerprintStore, KvStore, META_FACTORY_FROZEN,
    PortResult, RunStore, TaskStore,
};
use aaf_domain::task::TaskState;
use serde::Serialize;

pub use audit_backend::{
    AuditBackend, audit_cmd_is_claude, build_audit_command, configured_audit_provider,
    effective_audit_provider, resolve_audit_policy, same_audit_provider,
};
pub use git_real::{BaseRef, RealGit, resolve_base};
pub use ports::{AccountPool, GateRunner, GitOps, Grant, Integration, Notifier, Purpose};
pub use post_audit::is_large_diff;
pub use scheduler::{
    Hooks, MAX_TRANSIENT_BACKOFF_SECS, MAX_TRANSIENT_RETRIES, run, run_with_audit,
    transient_backoff_secs,
};
pub use select_pool::{BindingView, SelectingPool};

/// Every store the loop needs, as one object so a single `Db` handle serves
/// all of them.
pub trait Store:
    TaskStore + BudgetStore + RunStore + EventSink + KvStore + FingerprintStore
{
    fn record_turn_snapshot(
        &self,
        _task_id: &str,
        _run_id: &str,
        _ref_name: &str,
        _commit_oid: &str,
        _now: i64,
    ) -> PortResult<()> {
        Ok(())
    }
}

impl Store for crate::db::Db {
    fn record_turn_snapshot(
        &self,
        task_id: &str,
        run_id: &str,
        ref_name: &str,
        commit_oid: &str,
        now: i64,
    ) -> PortResult<()> {
        self.insert_snapshot(task_id, run_id, ref_name, commit_oid, now)
            .map(|_| ())
    }
}

/// What one `aaf run` was asked to do, already merged from config and flags.
#[derive(Debug, Clone)]
pub struct Settings {
    /// `tasks.repo_root` of the repository this loop serves.
    pub repo_root: String,
    pub repo: RepoConfig,
    pub max_parallel: u32,
    pub auto_merge: bool,
    /// False on Linux or with `--unsandboxed` (PLAN A.17).
    pub sandboxed: bool,
    /// The executor is Claude Code, whose tool calls `aaf-hook` judges: an
    /// agent is started only where `.claude/settings.json` registers the
    /// hook for every guarded tool (PLAN C.3). Other executors have no such
    /// hook to require.
    pub hook_required: bool,
    /// The PATH the agents are started with: the registered hook has to be
    /// found on it, or it ends with exit 127 and blocks nothing.
    pub agent_path: OsString,
    /// Adds the store-policy items to the cross-audit checklist.
    pub mobile: bool,
    pub data_dir: PathBuf,
    pub home: PathBuf,
}

pub struct Deps {
    pub store: Arc<dyn Store>,
    pub clock: Arc<dyn Clock>,
    pub runner: Arc<dyn AgentRunner>,
    pub accounts: Arc<dyn AccountPool>,
    pub git: Arc<dyn GitOps>,
    pub gate: Arc<dyn GateRunner>,
    pub notifier: Arc<dyn Notifier>,
    pub settings: Settings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Every task gets at most one agent run, then the loop returns.
    Once,
    /// Keeps looking for Ready tasks until `Hooks::idle` says stop or the
    /// factory freezes.
    Auto,
}

/// What one call of [`run`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RunSummary {
    pub agent_runs: u32,
    pub gate_runs: u32,
    /// Task ids by where this run left them.
    pub done: Vec<String>,
    pub review: Vec<String>,
    pub blocked: Vec<String>,
    /// Sent back to Ready by the unblocker (retry, account cooldown).
    pub requeued: Vec<String>,
    /// Jobs that could not start because no account was free.
    pub waiting_for_account: u32,
    /// Ready tasks held back by the `run.max_open_review` queue brake.
    pub waiting_for_review_cap: u32,
    /// Ready tasks waiting on a `Blocked` upstream dependency (`(task_id, blocker_id)`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub blocked_upstream: Vec<(String, String)>,
    pub frozen: bool,
    /// Why the run stopped taking tasks: the agent CLI failed for its
    /// account or the network (`AgentOutcome::infra`). Every further task
    /// would fail the same way, so none was started after it; the task it
    /// happened on is back in Ready, not charged with an attempt.
    pub halted: Option<String>,
    /// Infrastructure errors. Each one blocked its task, except a pull
    /// request that could not be opened: that task is in `review`.
    pub errors: Vec<String>,
}

/// True while `meta.factory_frozen` is set (`aaf board unfreeze` clears it).
pub fn is_frozen(store: &dyn KvStore) -> PortResult<bool> {
    Ok(store.get_meta(META_FACTORY_FROZEN)?.as_deref() == Some("1"))
}

/// `aaf board unfreeze`: clears the freeze flag and the consecutive-failure
/// counter, and records it as an event.
pub fn unfreeze(store: &dyn Store, clock: &dyn Clock) -> PortResult<()> {
    // The ladder's limits play no part in unfreezing.
    let cfg = aaf_domain::config::UnblockerConfig::default();
    crate::unblocker::Ladder::new(store, store, store, store, clock, &cfg).unfreeze()
}

impl Deps {
    pub(crate) fn now(&self) -> i64 {
        self.clock.now()
    }

    pub(crate) fn emit(
        &self,
        kind: EventKind,
        task_id: &str,
        payload: serde_json::Value,
    ) -> PortResult<()> {
        let event = FactoryEvent::new(kind, payload.to_string(), self.now()).for_task(task_id);
        self.store.emit(&event).map(|_| ())
    }

    /// Moves the task and records the move, so `aaf board show` can tell why.
    pub(crate) fn move_to(
        &self,
        task_id: &str,
        state: TaskState,
        blocked: Option<&BlockedReason>,
        why: &str,
    ) -> PortResult<()> {
        self.store.set_state(task_id, state, blocked, self.now())?;
        self.emit(
            EventKind::TaskStateChanged,
            task_id,
            serde_json::json!({ "to": state.as_str(), "why": why }),
        )
    }
}
