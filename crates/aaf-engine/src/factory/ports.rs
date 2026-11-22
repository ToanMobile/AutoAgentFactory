//! The factory's own seams: gate, git, account pool and notifications.
//! Production implementations live in `adapters` and `git_real`; tests use
//! in-memory fakes.

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use aaf_domain::account::AccountProfile;
use aaf_domain::gate::{HeadLockReport, Receipt};
use aaf_domain::ports::{LimitHit, PortResult};

use crate::git::{GitError, MergeOutcome, PrOutcome, TaskWorktree};
use crate::unblocker::QuotaRoute;
use crate::webhook::WebhookEvent;

pub trait GateRunner: Send + Sync {
    /// Runs the gate once on `tree`: a task worktree or an integration
    /// worktree holding several task branches. May block for a long time
    /// (machine load control); that wait is not a failure.
    fn run(&self, tree: &Path) -> PortResult<Receipt>;
    /// Head-lock state of `tree` without running any command.
    fn head_lock(&self, tree: &Path) -> PortResult<HeadLockReport>;
}

/// An integration worktree: the base plus every branch that merged cleanly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Integration {
    pub tree: TaskWorktree,
    /// Task ids whose branch is in the tree.
    pub merged: Vec<String>,
    /// Task ids whose branch conflicted and was left out.
    pub conflicted: Vec<String>,
}

pub trait GitOps: Send + Sync {
    /// The task's worktree on `aaf/<task_id>`: created from the base on
    /// first use, returned as it is afterwards.
    fn task_worktree(&self, task_id: &str) -> PortResult<TaskWorktree>;
    /// Commits whatever the agent left uncommitted.
    fn commit_all(&self, worktree: &TaskWorktree, message: &str) -> PortResult<()>;
    /// True when the branch differs from the base.
    fn has_changes(&self, worktree: &TaskWorktree) -> PortResult<bool>;
    /// The branch's diff against the base, for the cross-audit.
    fn diff(&self, worktree: &TaskWorktree) -> PortResult<String>;
    /// Merges the tasks' branches into a fresh integration worktree.
    fn integrate(&self, task_ids: &[String]) -> PortResult<Integration>;
    fn discard(&self, integration: &Integration) -> PortResult<()>;
    /// Rebases the branch onto the base. Returns the conflicting files after
    /// aborting; empty means the rebase succeeded.
    fn rebase(&self, worktree: &TaskWorktree) -> PortResult<Vec<String>>;
    /// Current tip of the base branch.
    fn base_sha(&self) -> PortResult<String>;
    /// Lands the branch through the merge queue; `regate` judges the rebased
    /// tree.
    fn merge(
        &self,
        worktree: &TaskWorktree,
        regate: &dyn Fn(&Path) -> Result<bool, String>,
    ) -> PortResult<MergeOutcome>;
    /// `note` goes above the receipt summary in the pull request body: it
    /// says whose receipt this is when the gate ran on an integration tree.
    fn open_pr(
        &self,
        worktree: &TaskWorktree,
        title: &str,
        receipt: &Receipt,
        note: Option<&str>,
    ) -> PortResult<PrOutcome>;
    /// Refreshes the local view of the base after a remote merge.
    fn fetch_base(&self) -> PortResult<()>;
    /// True when the task's branch is contained in the base.
    fn is_merged(&self, task_id: &str) -> PortResult<bool>;
    /// Removes the worktree and branch of a task that is done. Never
    /// discards work: a dirty worktree or an unmerged branch is an error.
    fn cleanup(&self, task_id: &str) -> PortResult<()>;
    /// Existing worktree and its `HEAD` oid (without creating a worktree).
    fn existing_worktree_head(
        &self,
        _task_id: &str,
    ) -> Result<Option<(TaskWorktree, String)>, GitError> {
        Ok(None)
    }
    /// Removes orphaned `aaf/batch-*` worktrees/branches not in `active_task_ids`.
    fn sweep_orphan_batches(&self, _active_task_ids: &HashSet<String>) -> Result<usize, GitError> {
        Ok(0)
    }
    /// Drops stale git worktree registrations whose directories are gone.
    fn prune_worktrees(&self) -> Result<(), GitError> {
        Ok(())
    }
    /// Checks `(task_id, local_head_oid)` candidates against merged GitHub PRs
    /// and local git ancestry. Returns verified `task_id -> expected_head_oid`.
    fn verified_squash_merges(
        &self,
        _candidates: &[(String, String)],
    ) -> Result<HashMap<String, String>, GitError> {
        Ok(HashMap::new())
    }
    /// Removes a worktree/branch/snapshot refs after squash-merge verification.
    fn remove_verified_merged(
        &self,
        worktree: &TaskWorktree,
        _expected_head_oid: &str,
    ) -> Result<(), GitError> {
        let task_id = worktree.task_id()?;
        self.cleanup(task_id)
            .map_err(|e| GitError::Invalid(e.to_string()))
    }
    /// Creates a pre-turn git snapshot ref (`refs/aaf/snapshots/<task_id>/<ts>-<run_id>`)
    /// pointing at `worktree`'s current `HEAD`.
    fn create_turn_snapshot(
        &self,
        _worktree: &TaskWorktree,
        _run_id: &str,
        _now: i64,
    ) -> Result<Option<(String, String)>, GitError> {
        Ok(None)
    }
}

/// An account to run on, held until dropped. `account` is `None` when no
/// account is configured at all (the agent then uses its default login).
pub struct Grant {
    pub account: Option<AccountProfile>,
    /// The lease; only its `Drop` matters.
    _lease: Option<Box<dyn Any + Send>>,
}

impl Grant {
    pub fn new(account: Option<AccountProfile>, lease: Option<Box<dyn Any + Send>>) -> Grant {
        Grant {
            account,
            _lease: lease,
        }
    }

    pub fn account_id(&self) -> Option<&str> {
        self.account.as_ref().map(|a| a.id.as_str())
    }
}

/// What a job needs its account for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// The task's own agent run, or an unblock step of it.
    Work,
    /// The cross-audit of the task's changes: independent of the author's
    /// account where the selection mode leaves a choice.
    Audit,
}

pub trait AccountPool: Send + Sync {
    /// `None` when every account is cooling down or leased: the caller
    /// waits, it does not fail.
    fn acquire(&self) -> PortResult<Option<Grant>>;
    /// [`AccountPool::acquire`] for a job of `task_id`. A pool that binds
    /// repositories or tasks to accounts answers by that binding, and
    /// `None` then also means "its account is busy or cooling down".
    fn acquire_for(&self, _task_id: &str, _purpose: Purpose) -> PortResult<Option<Grant>> {
        self.acquire()
    }
    /// What a task whose account just hit its limit can do; `None` when the
    /// pool cannot tell (the unblocker then guesses from the repeat count).
    fn after_limit(&self) -> PortResult<Option<QuotaRoute>> {
        Ok(None)
    }
    fn report_limit(&self, account_id: &str, hit: &LimitHit) -> PortResult<()>;
    /// Every known account, for the sandbox's read-deny list.
    fn profiles(&self) -> PortResult<Vec<AccountProfile>>;
    /// Number of configured accounts matching this pool's provider.
    fn provider_account_count(&self) -> PortResult<usize> {
        Ok(self.profiles()?.len())
    }
}

pub trait Notifier: Send + Sync {
    /// `Ok(false)` when notifications are turned off.
    fn notify(&self, event: &WebhookEvent) -> Result<bool, String>;
}
