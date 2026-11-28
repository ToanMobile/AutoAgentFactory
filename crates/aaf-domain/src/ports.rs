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
    /// Atomically transitions `id` from `expected` to `next`, rejecting illegal
    /// moves via [`TaskState::can_transition_to`].
    ///
    /// Returns `Ok(true)` when the task was in `expected` and moved to `next`,
    /// or `Ok(false)` when its current state differed from `expected`.
    fn compare_and_set_state(
        &self,
        id: &str,
        expected: TaskState,
        next: TaskState,
        blocked_reason: Option<&BlockedReason>,
        now: i64,
    ) -> PortResult<bool> {
        if self.get_task(id)?.state != expected {
            return Ok(false);
        }
        self.set_state(id, next, blocked_reason, now)?;
        Ok(true)
    }
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
    /// Sets persisted retry scheduling for a transient `Blocked` task.
    fn set_retry_schedule(
        &self,
        _id: &str,
        _retry_after: Option<i64>,
        _auto_recover_count: u32,
        _now: i64,
    ) -> PortResult<()> {
        Ok(())
    }
    /// Transitions due transient `Blocked` tasks (`retry_after <= now`) back to
    /// `Ready` without appending an `"unblocked:"` note or resetting ladder markers.
    fn wake_due_blocked_tasks(&self, _repo_root: &str, _now: i64) -> PortResult<Vec<String>> {
        Ok(Vec::new())
    }
}

pub trait BudgetStore: Send + Sync {
    fn put_budget(&self, budget: &RootBudget) -> PortResult<()>;
    fn get_budget(&self, root_id: &str) -> PortResult<RootBudget>;
    /// Atomically inserts `budget` only if missing, monotonically raising
    /// `token_limit` and `wall_secs_limit` on unfrozen, non-exhausted existing
    /// rows when `budget` has higher limits, without clobbering concurrent
    /// `add_usage` writes.
    fn ensure_budget_if_missing(&self, budget: &RootBudget) -> PortResult<RootBudget> {
        match self.get_budget(&budget.root_id) {
            Ok(mut existing) => {
                if existing.check() == crate::budget::BudgetCheck::Ok
                    && (existing.token_limit < budget.token_limit
                        || existing.wall_secs_limit < budget.wall_secs_limit)
                {
                    existing.token_limit = existing.token_limit.max(budget.token_limit);
                    existing.wall_secs_limit = existing.wall_secs_limit.max(budget.wall_secs_limit);
                    self.put_budget(&existing)?;
                }
                Ok(existing)
            }
            Err(PortError::NotFound(_)) => {
                self.put_budget(budget)?;
                Ok(budget.clone())
            }
            Err(err) => Err(err),
        }
    }
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
    /// Newest first, filtered to `kinds`.
    fn events_for_task_by_kinds(
        &self,
        task_id: &str,
        kinds: &[crate::event::EventKind],
        limit: u32,
    ) -> PortResult<Vec<FactoryEvent>> {
        let all = self.events_for_task(task_id, u32::MAX)?;
        Ok(all
            .into_iter()
            .filter(|ev| kinds.contains(&ev.kind))
            .take(limit as usize)
            .collect())
    }
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
/// `meta` key: Unix seconds of the last `aaf board unfreeze`, anchoring the 1h
/// token-spike window so pre-unfreeze spikes do not immediately re-freeze.
pub const META_FACTORY_UNFROZEN_AT: &str = "factory_unfrozen_at";
/// `meta` key: estimated resume Unix timestamp while paused by the 24h token
/// cap (`"0"` or absent when not paused); never shares state with `META_FACTORY_FROZEN`.
pub const META_COST_PAUSED_UNTIL: &str = "cost_paused_until";
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
    /// Optional inline JSON schema string for structured output enforcement.
    #[serde(default)]
    pub json_schema: Option<String>,
    /// Optional upper bound on agentic turns for executors that support it
    /// (e.g. Claude `--max-turns`). Ignored cleanly by executors without turn caps.
    #[serde(default)]
    pub max_turns: Option<u32>,
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
    /// A sandboxed run hit a mid-run OAuth Keychain write denial, and the host
    /// verified a fresh OAuth token was saved afterward; safe to retry once
    /// without quarantining the account or halting `Mode::Drain`.
    OAuthRefreshed,
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

    /// Whether this runner measures and reports token usage in [`AgentOutcome`].
    /// Defaults to `true`; generic CLI runners that do not parse token counts
    /// override this to `false`.
    fn reports_tokens(&self) -> bool {
        true
    }
}
