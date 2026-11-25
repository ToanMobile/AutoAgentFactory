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
mod audit_turn;
pub mod batch;
mod commit_lock;
pub mod dual_audit;
mod finish;
mod finish_land;
mod git_real;
mod git_real_base;
mod infra_retry;
mod merge_retry;
pub(crate) mod plan_job;
mod plan_legacy;
pub mod ports;
mod post_audit;
mod pr_feedback;
mod pr_feedback_note;
pub mod prompt;
mod rework_filter;
pub mod schedule_order;
mod scheduler;
mod select_pool;
pub mod snapshots;
pub mod stage_router;
mod sweep;
mod sweep_squash;
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

pub use crate::git::base_ref::{BaseRef, resolve_base};
pub use audit_backend::{
    AuditBackend, audit_cmd_is_claude, build_audit_command, configured_audit_provider,
    effective_audit_provider, resolve_audit_policy, resolve_auto_audit_executor,
    same_audit_provider,
};
pub use git_real::RealGit;
pub use ports::{
    AccountPool, GateRunner, GitOps, Grant, Integration, IntegrationGuard, Notifier, Purpose,
};
pub use post_audit::is_large_diff;
pub use scheduler::{
    Hooks, MAX_TRANSIENT_BACKOFF_SECS, MAX_TRANSIENT_RETRIES, run, run_with_audit,
    run_with_audit_and_stop, run_with_router, run_with_router_and_stop, transient_backoff_secs,
};
pub use select_pool::{BindingView, SelectingPool};
pub use stage_router::{
    AntigravityLockedPool, ProviderKey, StageRouter, StageTarget, any_stage_uses_claude,
    classify_tier, is_high_risk_path, is_tier_s_skip_debate, rework_count,
};

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

    /// Persists a task split (budget if absent, child tasks, DAG deps, creation
    /// events, and parent context note). [`crate::db::Db`] executes all steps
    /// in a single SQLite write transaction; in-memory stores fall back to
    /// sequential port calls.
    #[allow(clippy::too_many_arguments)]
    fn apply_split_atomic(
        &self,
        parent_id: &str,
        budget: &aaf_domain::budget::RootBudget,
        children: &[aaf_domain::task::Task],
        deps: &[(String, String)],
        events: &[FactoryEvent],
        parent_note: &str,
        now: i64,
    ) -> PortResult<()> {
        match self.get_budget(&budget.root_id) {
            Ok(_) => {}
            Err(aaf_domain::ports::PortError::NotFound(_)) => self.put_budget(budget)?,
            Err(err) => return Err(err),
        }
        for child in children {
            self.insert_task(child)?;
        }
        for (task_id, blocked_by) in deps {
            self.add_dep(task_id, blocked_by)?;
        }
        for ev in events {
            self.emit(ev)?;
        }
        self.append_context_note(parent_id, parent_note, now)
    }

    /// Atomically transitions a `Review` task to `Done`, emitting
    /// `TaskStateChanged` (and optional `MergeResult`), recording any closed
    /// fingerprint, and resetting `consecutive_root_fails` for root tasks.
    ///
    /// [`crate::db::Db`] runs all steps inside a single `BEGIN IMMEDIATE`
    /// SQLite transaction; in-memory stores fall back to `compare_and_set_state`
    /// followed by sequential port calls.
    fn complete_review_task(
        &self,
        task_id: &str,
        why: &str,
        merge_payload: Option<&serde_json::Value>,
        now: i64,
    ) -> PortResult<bool> {
        if !self.compare_and_set_state(task_id, TaskState::Review, TaskState::Done, None, now)? {
            return Ok(false);
        }
        let task = self.get_task(task_id)?;
        self.emit(
            &FactoryEvent::new(
                EventKind::TaskStateChanged,
                serde_json::json!({ "to": TaskState::Done.as_str(), "why": why }).to_string(),
                now,
            )
            .for_task(task_id),
        )?;
        if let Some(payload) = merge_payload {
            self.emit(
                &FactoryEvent::new(EventKind::MergeResult, payload.to_string(), now)
                    .for_task(task_id),
            )?;
        }
        if let Some(fp) = &task.fingerprint {
            self.record_closed(&aaf_domain::ports::ClosedFingerprint {
                fingerprint: fp.clone(),
                repo_root: task.repo_root.clone(),
                task_id: task.id.clone(),
                closed_at: now,
            })?;
        }
        if task.id == task.root_id {
            self.set_meta(aaf_domain::ports::META_CONSECUTIVE_ROOT_FAILS, "0")?;
        }
        Ok(true)
    }

    /// Returns `true` when any recorded run in `repo_root` covers timestamp `at`
    /// (`started_at <= at <= ended_at.unwrap_or(now)`).
    fn has_overlapping_run(&self, repo_root: &str, at: i64, now: i64) -> PortResult<bool> {
        for task in self.list_tasks(Some(repo_root), None)? {
            for run in self.runs_for_task(&task.id)? {
                let end = run.ended_at.unwrap_or(now);
                if run.started_at <= at && at <= end {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Returns `(total_tokens, earliest_nonzero_ts)` across every run on this
    /// machine with `COALESCE(ended_at, started_at) >= since`.
    fn cost_window_usage(&self, since: i64) -> PortResult<(u64, Option<i64>)> {
        let mut total: u64 = 0;
        let mut earliest: Option<i64> = None;
        for task in self.list_tasks(None, None)? {
            for run in self.runs_for_task(&task.id)? {
                let ts = run.ended_at.unwrap_or(run.started_at);
                if ts >= since {
                    let tokens = run.tokens_in.saturating_add(run.tokens_out);
                    total = total.saturating_add(tokens);
                    if tokens > 0 {
                        earliest = Some(earliest.map_or(ts, |e| e.min(ts)));
                    }
                }
            }
        }
        Ok((total, earliest))
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

    fn apply_split_atomic(
        &self,
        parent_id: &str,
        budget: &aaf_domain::budget::RootBudget,
        children: &[aaf_domain::task::Task],
        deps: &[(String, String)],
        events: &[FactoryEvent],
        parent_note: &str,
        now: i64,
    ) -> PortResult<()> {
        crate::db::Db::apply_split_atomic(
            self,
            parent_id,
            budget,
            children,
            deps,
            events,
            parent_note,
            now,
        )
    }

    fn complete_review_task(
        &self,
        task_id: &str,
        why: &str,
        merge_payload: Option<&serde_json::Value>,
        now: i64,
    ) -> PortResult<bool> {
        crate::db::Db::complete_review_task(self, task_id, why, merge_payload, now)
    }

    fn has_overlapping_run(&self, repo_root: &str, at: i64, now: i64) -> PortResult<bool> {
        crate::db::Db::has_overlapping_run(self, repo_root, at, now)
    }

    fn cost_window_usage(&self, since: i64) -> PortResult<(u64, Option<i64>)> {
        crate::db::Db::cost_window_usage(self, since)
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
    /// Trusted GitHub logins from Tier-1 `[safety].owner_github_allowlist`
    /// allowed to trigger PR feedback rework via review threads.
    pub owner_github_allowlist: Vec<String>,
}

impl Settings {
    /// Effective landing strategy derived from `repo.run.land` and `auto_merge`.
    pub fn effective_land(&self) -> aaf_domain::config::LandMode {
        use aaf_domain::config::LandMode;
        if self.auto_merge {
            if self.repo.run.land.auto_lands() {
                self.repo.run.land
            } else {
                LandMode::Auto
            }
        } else if self.repo.run.auto_merge {
            LandMode::Pr
        } else {
            self.repo.run.land
        }
    }
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
    /// Drains runnable work (retries, audits, newly unblocked dependents) until
    /// no task can make forward progress right now, then returns immediately.
    Drain,
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
    pub cost_paused: bool,
    /// Why the run stopped taking tasks: the agent CLI failed for its
    /// account or the network (`AgentOutcome::infra`). Every further task
    /// would fail the same way, so none was started after it; the task it
    /// happened on is back in Ready, not charged with an attempt.
    pub halted: Option<String>,
    /// Infrastructure errors and warnings. A task error blocked its task,
    /// except a pull request that could not be opened: that task is in
    /// `review`. Startup-sweep warnings (e.g. orphan batch worktrees kept)
    /// block nothing.
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
