//! The subtask proposer's agent runs, recorded like any other run.

use aaf_domain::event::EventKind;
use aaf_domain::ports::{AgentOutcome, AgentRequest, AgentRunner, PortResult};
use aaf_domain::run::{RunFinish, RunRecord, RunStatus};
use aaf_domain::task::Task;
use serde_json::json;

use super::super::Deps;
use super::super::worker::{ensure_budget, status_of};
use crate::unblocker::PROPOSER_EXECUTOR;

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
        self.deps.store.add_usage(&self.task.root_id, 0, wall, 0)?;
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
