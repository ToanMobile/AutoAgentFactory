//! What the scheduler does with the outcome of a worker and the verdict of
//! the gate thread, and the gate thread itself.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender};

use aaf_domain::ports::PortResult;
use aaf_domain::task::TaskState;

use super::{GateStats, MAX_GATE_RETRIES, Msg, Scheduler, judge_round};
use crate::factory::finish::Resolution;
use crate::factory::worker::{self, GateJob, Job, Next, Outcome};
use crate::factory::{Deps, unblock};

impl Scheduler<'_> {
    pub(super) fn clear_gating(&mut self, task_id: &str) {
        self.gating_in_flight.remove(task_id);
        self.state.gate_retries.remove(task_id);
        self.state.pre_audits.remove(task_id);
    }

    pub(super) fn handle_worker(&mut self, outcome: Outcome) -> PortResult<()> {
        self.state.active -= 1;
        self.summary.errors.extend(outcome.error);
        if !matches!(
            outcome.next,
            Next::Gate(_)
                | Next::GateAudited(..)
                | Next::Unblock { .. }
                | Next::SwitchJob(_)
                | Next::RetryAudit(_)
        ) {
            self.gating_in_flight.remove(&outcome.task_id);
        }
        if !matches!(outcome.next, Next::GateAudited(..) | Next::SwitchJob(_)) {
            self.state.pre_audits.remove(&outcome.task_id);
        }
        match outcome.next {
            Next::Gate(job) => {
                self.state.transient_retries.remove(&outcome.task_id);
                self.queue_gate(job)?;
            }
            Next::GateAudited(job, pre) => {
                self.state.pre_audits.insert(outcome.task_id.clone(), pre);
                self.state.transient_retries.remove(&outcome.task_id);
                self.queue_gate(job)?;
            }
            Next::Unblock {
                task_id,
                worktree,
                signal,
            } => self.state.jobs.push_back(Job::Unblock {
                job: GateJob {
                    task_id,
                    worktree,
                    audited: false,
                    cached_receipt: None,
                },
                signal,
            }),
            Next::Ready => {
                self.state.transient_retries.remove(&outcome.task_id);
                self.summary.requeued.push(outcome.task_id);
            }
            Next::Switch => self.handle_switch(outcome.task_id)?,
            Next::SwitchJob(job) => self.handle_switch_job(outcome.task_id, *job)?,
            Next::RetryAudit(job) => self.state.jobs.push_back(*job),
            Next::Wait { until } => self.handle_wait(outcome.task_id, until)?,
            Next::RetryAfter {
                until,
                attempt,
                detail,
            } => self.handle_retry_after(outcome.task_id, until, attempt, detail)?,
            Next::Halt(why) => {
                let state = self.deps.store.get_task(&outcome.task_id)?.state;
                self.requeue_if_ready(outcome.task_id, state);
                self.summary.halted.get_or_insert(why);
            }
            Next::Blocked => {
                self.state.transient_retries.remove(&outcome.task_id);
                self.summary.blocked.push(outcome.task_id);
            }
        }
        Ok(())
    }

    pub(super) fn handle_gate(
        &mut self,
        job: GateJob,
        resolution: Result<Resolution, String>,
    ) -> PortResult<()> {
        self.state.in_gate -= 1;
        match resolution {
            Ok(Resolution::Settled(state)) => {
                self.clear_gating(&job.task_id);
                self.state.transient_retries.remove(&job.task_id);
                match state {
                    TaskState::Done => self.summary.done.push(job.task_id),
                    TaskState::Review => self.summary.review.push(job.task_id),
                    _ => self.summary.blocked.push(job.task_id),
                }
            }
            Ok(Resolution::NeedsAudit { lock_fp }) => {
                self.state.pre_audits.remove(&job.task_id);
                self.state.jobs.push_back(Job::Audit { job, lock_fp });
            }
            Ok(Resolution::NeedsPostAudit { receipt }) => {
                let pre_audit = self.state.pre_audits.remove(&job.task_id);
                self.state.jobs.push_back(Job::PostAudit {
                    job,
                    receipt,
                    pre_audit,
                });
            }
            Ok(Resolution::Failed(signal)) => {
                self.state.pre_audits.remove(&job.task_id);
                self.state.jobs.push_back(Job::Unblock { job, signal });
            }
            Ok(Resolution::Retry) => {
                let count = self
                    .state
                    .gate_retries
                    .entry(job.task_id.clone())
                    .or_default();
                *count += 1;
                if *count > MAX_GATE_RETRIES {
                    self.clear_gating(&job.task_id);
                    let why =
                        format!("gate/merge retried {MAX_GATE_RETRIES} times without landing");
                    unblock::block_untried(self.deps, &job.task_id, &why)?;
                    self.summary.blocked.push(job.task_id);
                } else {
                    self.queue_gate(job)?;
                }
            }
            Err(error) => {
                self.clear_gating(&job.task_id);
                let outcome = worker::block_infra(self.deps, &job.task_id, &error);
                self.summary.errors.extend(outcome.error);
                self.summary.blocked.push(job.task_id);
            }
        }
        Ok(())
    }
}

pub(super) fn gate_loop(
    deps: &Deps,
    queue: &Receiver<Vec<GateJob>>,
    tx: &Sender<Msg>,
    stats: &GateStats,
) {
    while let Ok(mut jobs) = queue.recv() {
        jobs.extend(queue.try_iter().flatten());
        let pending = Mutex::new(jobs.clone());
        let round = catch_unwind(AssertUnwindSafe(|| {
            judge_round(deps, jobs, stats, &|job, resolution| {
                if let Ok(mut pending) = pending.lock() {
                    pending.retain(|p| p.task_id != job.task_id);
                }
                let _ = tx.send(Msg::Gate(job, resolution));
            });
        }));
        if round.is_err() {
            let left = pending.lock().map(|mut p| std::mem::take(&mut *p));
            for job in left.unwrap_or_default() {
                let _ = tx.send(Msg::Gate(job, Err("the gate thread panicked".to_string())));
            }
        }
    }
}
