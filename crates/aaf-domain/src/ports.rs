//! Port traits. The engine's SQLite store and executors implement these;
//! everything else depends only on the traits and is tested with fakes.

use serde::{Deserialize, Serialize};

use crate::account::{AccountProfile, AccountState};
use crate::blocked::BlockedReason;
use crate::budget::RootBudget;
use crate::event::FactoryEvent;
use crate::run::{RunFinish, RunRecord};
use crate::task::{Task, TaskState};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PortError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("io: {0}")]
    Io(String),
}

pub type PortResult<T> = Result<T, PortError>;

/// Source of the current time, so tests never read the real clock.
pub trait Clock: Send + Sync {
    /// Unix seconds.
    fn now(&self) -> i64;
}

pub trait TaskStore: Send + Sync {
    fn insert_task(&self, task: &Task) -> PortResult<()>;
    fn get_task(&self, id: &str) -> PortResult<Task>;
    /// Both filters are optional; results are ordered by priority, then age.
    fn list_tasks(
        &self,
        repo_root: Option<&str>,
        state: Option<TaskState>,
    ) -> PortResult<Vec<Task>>;
    /// Rejects moves that [`TaskState::can_transition_to`] forbids.
    /// `blocked` is stored when `state` is `Blocked` and cleared otherwise.
    fn set_state(
        &self,
        id: &str,
        state: TaskState,
        blocked: Option<&BlockedReason>,
        now: i64,
    ) -> PortResult<()>;
    fn append_context_note(&self, id: &str, note: &str, now: i64) -> PortResult<()>;
    /// Returns the new cycle count.
    fn increment_unblock_cycles(&self, id: &str, now: i64) -> PortResult<u32>;
    /// Rejects an edge that would create a cycle with `PortError::Conflict`.
    fn add_dep(&self, task_id: &str, blocked_by: &str) -> PortResult<()>;
    /// Removes a dependency edge; returns `true` if the edge existed.
    fn remove_dep(&self, task_id: &str, blocked_by: &str) -> PortResult<bool>;
    /// Ids of the tasks that block `task_id`.
    fn deps_of(&self, task_id: &str) -> PortResult<Vec<String>>;
    /// All `(task_id, blocked_by)` dependency edges across the store.
    fn all_deps(&self) -> PortResult<Vec<(String, String)>>;
    fn children_of(&self, parent_id: &str) -> PortResult<Vec<Task>>;
}

pub trait BudgetStore: Send + Sync {
    fn put_budget(&self, budget: &RootBudget) -> PortResult<()>;
    fn get_budget(&self, root_id: &str) -> PortResult<RootBudget>;
    /// Atomically adds usage and returns the updated budget.
    fn add_usage(
        &self,
        root_id: &str,
        tokens: u64,
        wall_secs: u64,
        cycles: u32,
    ) -> PortResult<RootBudget>;
    fn set_frozen(&self, root_id: &str, frozen: bool) -> PortResult<()>;
}

pub trait RunStore: Send + Sync {
    fn insert_run(&self, run: &RunRecord) -> PortResult<()>;
    fn finish_run(&self, id: &str, finish: &RunFinish) -> PortResult<()>;
    fn runs_for_task(&self, task_id: &str) -> PortResult<Vec<RunRecord>>;
    /// Sum of `tokens_in + tokens_out` for runs of `account_id` that started
    /// at or after `since`.
    fn tokens_since(&self, account_id: &str, since: i64) -> PortResult<u64>;
}

pub trait AccountStore: Send + Sync {
    /// Inserts the account or refreshes its provider and config dir; an
    /// existing cooldown is kept.
    fn upsert_account(&self, profile: &AccountProfile) -> PortResult<()>;
    fn list_accounts(&self) -> PortResult<Vec<AccountState>>;
    fn set_cooldown(&self, account_id: &str, until: Option<i64>) -> PortResult<()>;
}

pub trait EventSink: Send + Sync {
    /// Appends the event and returns its id.
    fn emit(&self, event: &FactoryEvent) -> PortResult<i64>;
    /// Newest first.
    fn events_for_task(&self, task_id: &str, limit: u32) -> PortResult<Vec<FactoryEvent>>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClosedFingerprint {
    pub fingerprint: String,
    pub repo_root: String,
    pub task_id: String,
    pub closed_at: i64,
}

pub trait FingerprintStore: Send + Sync {
    fn record_closed(&self, closed: &ClosedFingerprint) -> PortResult<()>;
    /// Most recent closure of `fingerprint` at or after `since`.
    fn find_closed_since(
        &self,
        fingerprint: &str,
        repo_root: &str,
        since: i64,
    ) -> PortResult<Option<ClosedFingerprint>>;
    /// Id of a non-terminal task that already carries this fingerprint.
    fn find_open_task(&self, fingerprint: &str, repo_root: &str) -> PortResult<Option<String>>;
}

/// Small key/value store for factory-wide flags and counters.
pub trait KvStore: Send + Sync {
    fn get_meta(&self, key: &str) -> PortResult<Option<String>>;
    fn set_meta(&self, key: &str, value: &str) -> PortResult<()>;
}

/// `meta` key: `"1"` while the whole factory is frozen.
pub const META_FACTORY_FROZEN: &str = "factory_frozen";
/// `meta` key: count of consecutive failed root tasks.
pub const META_CONSECUTIVE_ROOT_FAILS: &str = "consecutive_root_fails";

/// Filesystem confinement requested for one agent run.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SandboxPolicy {
    /// False runs the agent unconfined (explicit opt-in only).
    pub enabled: bool,
    /// True denies all writes to the worktree, `.git`, and build caches
    /// (only temp dirs and account session state remain writable).
    #[serde(default)]
    pub read_only: bool,
    pub writable_paths: Vec<String>,
    pub read_denied_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRequest {
    pub run_id: String,
    pub prompt: String,
    /// Working directory (the task's worktree).
    pub cwd: String,
    pub account: Option<AccountProfile>,
    pub timeout_secs: u64,
    pub sandbox: SandboxPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitHit {
    /// When the provider says the limit resets, if it said so.
    pub reset_at: Option<i64>,
    pub raw: String,
}

/// What kind of trouble outside the task stopped an agent run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InfraKind {
    /// The provider refused the account: no login, an expired or rejected
    /// credential, an account on hold or without billing.
    Auth,
    /// The provider could not be reached (DNS, connection, proxy).
    Network,
}

/// A run that failed for a reason of the machine or the account, not of the
/// task: another attempt at the task fails the same way until a person
/// fixes the cause.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InfraFailure {
    pub kind: InfraKind,
    /// The executor's own words.
    pub raw: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentOutcome {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// The agent's final answer text.
    pub final_text: String,
    /// input + cache-creation tokens.
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub limit_hit: Option<LimitHit>,
    /// Set only from evidence the executor really has; `None` for an
    /// executor that cannot tell.
    #[serde(default)]
    pub infra: Option<InfraFailure>,
    pub log_tail: String,
}

impl AgentOutcome {
    pub fn succeeded(&self) -> bool {
        !self.timed_out
            && self.limit_hit.is_none()
            && self.infra.is_none()
            && self.exit_code == Some(0)
    }
}

/// Runs one agent to completion. Implemented by the executors; faked in tests.
pub trait AgentRunner: Send + Sync {
    fn run(&self, request: &AgentRequest) -> PortResult<AgentOutcome>;
}
