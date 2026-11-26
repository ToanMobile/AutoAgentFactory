//! The subtask proposer's agent runs, recorded like any other run.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use aaf_domain::account::ProviderKind;
use aaf_domain::blocked::{BlockedReason, SubtaskPlan};
use aaf_domain::event::EventKind;
use aaf_domain::ports::{AgentOutcome, AgentRequest, AgentRunner, PortError, PortResult};
use aaf_domain::run::{RunFinish, RunRecord, RunStatus};
use aaf_domain::task::Task;
use serde_json::json;

use super::super::ports::{AccountPool, Grant, Purpose};
use super::super::worker::{ensure_budget, request_with, should_charge_wall, status_of};
use super::super::{Deps, Store};
use crate::unblocker::{AgentSubtaskProposer, PROPOSER_EXECUTOR, SubtaskProposer};

/// Records T4 subtask-proposer runs in `runs`, emits `RunStarted`/`RunFinished`,
/// and adds wall-clock seconds to the root budget (`AgentSubtaskProposer` charges tokens).
pub(super) struct ProposerRecorder<'a> {
    pub(super) deps: &'a Deps,
    pub(super) task: &'a Task,
    pub(super) branch: String,
    pub(super) account_id: Option<&'a str>,
    pub(super) inner: &'a dyn AgentRunner,
}

impl AgentRunner for ProposerRecorder<'_> {
    fn run(&self, request: &AgentRequest) -> PortResult<AgentOutcome> {
        let started = self.deps.now();
        self.deps.store.insert_run(&RunRecord {
            id: request.run_id.clone(),
            task_id: self.task.id.clone(),
            account_id: self.account_id.map(str::to_string),
            executor: PROPOSER_EXECUTOR.to_string(),
            worktree_path: request.cwd.clone(),
            branch: self.branch.clone(),
            status: RunStatus::Running,
            exit_code: None,
            tokens_in: 0,
            tokens_out: 0,
            started_at: started,
            ended_at: None,
        })?;
        self.deps.emit(
            EventKind::RunStarted,
            &self.task.id,
            json!({
                "run_id": request.run_id,
                "executor": PROPOSER_EXECUTOR,
                "account": self.account_id,
            }),
        )?;
        let res = self.inner.run(request);
        let ended = self.deps.now();
        let wall = u64::try_from(ended - started).unwrap_or(0);
        let (status, exit_code, tokens_in, tokens_out) = match &res {
            Ok(outcome) => (
                status_of(outcome),
                outcome.exit_code,
                outcome.tokens_in,
                outcome.tokens_out,
            ),
            Err(_) => (RunStatus::Failed, None, 0, 0),
        };
        self.deps.store.finish_run(
            &request.run_id,
            &RunFinish {
                status,
                exit_code,
                tokens_in,
                tokens_out,
                ended_at: ended,
            },
        )?;
        ensure_budget(self.deps, &self.task.root_id)?;
        let charge_wall = matches!(&res, Ok(outcome) if should_charge_wall(outcome));
        if charge_wall && wall > 0 {
            self.deps.store.add_usage(&self.task.root_id, 0, wall, 0)?;
        }
        self.deps.emit(
            EventKind::RunFinished,
            &self.task.id,
            json!({
                "run_id": request.run_id,
                "status": status.as_str(),
                "tokens_in": tokens_in,
                "tokens_out": tokens_out,
                "tokens_measured": self.inner.reports_tokens(),
            }),
        )?;
        res
    }

    fn reports_tokens(&self) -> bool {
        self.inner.reports_tokens()
    }
}

/// Acquires an account lease lazily only when the ladder reaches T4 decomposition.
/// Deterministic T2/T3/Freeze/Escalate unblocks never touch the account pool.
pub(super) struct LazyProposer<'a> {
    pub(super) deps: &'a Deps,
    pub(super) task: &'a Task,
    pub(super) cwd: &'a str,
    pub(super) branch: &'a str,
    pub(super) provider_kind: Option<ProviderKind>,
    pub(super) hook_req: bool,
    pub(super) accounts: &'a dyn AccountPool,
    pub(super) watch: &'a dyn AgentRunner,
    pub(super) existing_grant: Option<&'a Grant>,
    pub(super) provider_cooling: bool,
    acquired: Mutex<Option<Grant>>,
    account_busy: AtomicBool,
}

impl<'a> LazyProposer<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        deps: &'a Deps,
        task: &'a Task,
        cwd: &'a str,
        branch: &'a str,
        provider_kind: Option<ProviderKind>,
        hook_req: bool,
        accounts: &'a dyn AccountPool,
        watch: &'a dyn AgentRunner,
        existing_grant: Option<&'a Grant>,
        provider_cooling: bool,
    ) -> Self {
        Self {
            deps,
            task,
            cwd,
            branch,
            provider_kind,
            hook_req,
            accounts,
            watch,
            existing_grant,
            provider_cooling,
            acquired: Mutex::new(None),
            account_busy: AtomicBool::new(false),
        }
    }

    pub(super) fn was_account_busy(&self) -> bool {
        self.account_busy.load(Ordering::Relaxed)
    }

    pub(super) fn take_grant(&self) -> Option<Grant> {
        self.acquired.lock().ok().and_then(|mut g| g.take())
    }

    fn run_with_grant(
        &self,
        grant: &Grant,
        task: &Task,
        reason: &BlockedReason,
        digest: &str,
    ) -> PortResult<Vec<SubtaskPlan>> {
        let recorder = ProposerRecorder {
            deps: self.deps,
            task: self.task,
            branch: self.branch.to_string(),
            account_id: grant.account_id(),
            inner: self.watch,
        };
        let mut base = request_with(
            self.deps,
            self.cwd,
            grant,
            self.provider_kind,
            self.hook_req,
            self.accounts,
        )?;
        base.sandbox.read_only = true;
        let proposer = AgentSubtaskProposer {
            runner: &recorder,
            budgets: &*self.deps.store as &dyn Store,
            base,
        };
        proposer.propose(task, reason, digest)
    }
}

impl SubtaskProposer for LazyProposer<'_> {
    fn propose(
        &self,
        task: &Task,
        reason: &BlockedReason,
        digest: &str,
    ) -> PortResult<Vec<SubtaskPlan>> {
        if let Some(grant) = self.existing_grant {
            return self.run_with_grant(grant, task, reason, digest);
        }
        if self.provider_cooling {
            self.account_busy.store(true, Ordering::Relaxed);
            return Err(PortError::Io("account_busy".to_string()));
        }
        let Some(grant) = self.accounts.acquire_for(&task.id, Purpose::Work)? else {
            self.account_busy.store(true, Ordering::Relaxed);
            return Err(PortError::Io("account_busy".to_string()));
        };
        let result = self.run_with_grant(&grant, task, reason, digest);
        if let Ok(mut slot) = self.acquired.lock() {
            *slot = Some(grant);
        }
        result
    }
}
