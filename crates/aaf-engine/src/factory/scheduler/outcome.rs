//! What the scheduler does with the outcome of a worker and the verdict of
//! the gate thread, and the gate thread itself.

use std::collections::HashSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::Scope;
use std::time::Duration;

use aaf_domain::blocked::BlockedReason;
use aaf_domain::budget::{BudgetCheck, RootBudget};
use aaf_domain::event::EventKind;
use aaf_domain::pipeline::PlanContract;
use aaf_domain::ports::{PortError, PortResult};
use aaf_domain::task::{Task, TaskState};
use serde_json::json;

use super::{GateStats, MAX_GATE_RETRIES, Msg, Scheduler, TickBoard, judge_round};
use crate::factory::finish::Resolution;
use crate::factory::ports::{AccountPool, Purpose};
use crate::factory::stage_router::{ProviderKey, StageRouter};
use crate::factory::worker::{self, GateJob, Job, Next, Outcome};
use crate::factory::{Deps, plan_job, schedule_order, unblock};
use crate::plan_debate::PLAN_DEBATE_STALLED_PREFIX;
use crate::plan_debate::state::{DebateNext, DebateRole};

impl Scheduler<'_> {
    pub(in crate::factory) fn provider_key_for(&self, is_audit: bool) -> ProviderKey {
        if is_audit {
            self.audit
                .map(|a| ProviderKey::from_executor(&a.provider))
                .unwrap_or_else(|| {
                    ProviderKey::from_executor(&self.deps.settings.repo.run.executor)
                })
        } else {
            self.router
                .map(StageRouter::default_provider_key)
                .unwrap_or_else(|| {
                    ProviderKey::from_executor(&self.deps.settings.repo.run.executor)
                })
        }
    }

    pub(in crate::factory) fn pool_for_provider(
        &self,
        provider: &ProviderKey,
        is_audit: bool,
    ) -> Arc<dyn AccountPool> {
        if is_audit
            && let Some(backend) = self.audit
            && ProviderKey::from_executor(&backend.provider) == *provider
        {
            return Arc::clone(&backend.accounts);
        }
        if let Some(router) = self.router
            && let Some(pool) = router.pool_for(provider)
        {
            return pool;
        }
        match (is_audit, self.audit) {
            (true, Some(backend)) => Arc::clone(&backend.accounts),
            _ => Arc::clone(&self.deps.accounts),
        }
    }

    pub(super) fn clear_gating(&mut self, task_id: &str) {
        self.gating_in_flight.remove(task_id);
        self.state.gate_retries.remove(task_id);
        self.state.pre_audits.remove(task_id);
    }

    pub(super) fn handle_worker(&mut self, mut outcome: Outcome) -> PortResult<()> {
        self.state.active -= 1;
        self.summary.errors.extend(outcome.error.take());
        if self
            .state
            .dual_review_in_flight
            .contains_key(&outcome.task_id)
        {
            return self.handle_dual_sibling_outcome(outcome);
        }
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
        let stopping = self.is_stopping();
        match outcome.next {
            Next::Gate(job) => {
                self.state.transient_retries.remove(&outcome.task_id);
                if stopping {
                    self.clear_gating(&outcome.task_id);
                } else {
                    self.queue_gate(job)?;
                }
            }
            Next::GateAudited(job, pre) => {
                self.state.transient_retries.remove(&outcome.task_id);
                if stopping {
                    self.clear_gating(&outcome.task_id);
                } else {
                    self.state.pre_audits.insert(outcome.task_id.clone(), pre);
                    self.queue_gate(job)?;
                }
            }
            Next::Unblock {
                task_id,
                worktree,
                signal,
            } => {
                if stopping {
                    self.clear_gating(&task_id);
                } else {
                    self.state.jobs.push_back(Job::Unblock {
                        job: GateJob {
                            task_id,
                            worktree,
                            audited: false,
                            cached_receipt: None,
                        },
                        signal,
                    });
                }
            }
            Next::Ready => {
                self.state.transient_retries.remove(&outcome.task_id);
                self.summary.requeued.push(outcome.task_id);
            }
            Next::Switch => {
                if stopping {
                    let state = self.deps.store.get_task(&outcome.task_id)?.state;
                    if state == TaskState::Gating {
                        self.clear_gating(&outcome.task_id);
                    } else {
                        self.requeue_if_ready(outcome.task_id, state);
                    }
                } else {
                    self.handle_switch(outcome.task_id, outcome.provider)?;
                }
            }
            Next::SwitchJob(job) => {
                if stopping {
                    let state = self.deps.store.get_task(&outcome.task_id)?.state;
                    self.clear_gating(&outcome.task_id);
                    self.requeue_if_ready(outcome.task_id, state);
                } else {
                    self.handle_switch_job(outcome.task_id, outcome.provider, *job)?;
                }
            }
            Next::RetryAudit(job) => {
                if stopping {
                    self.clear_gating(&outcome.task_id);
                } else {
                    self.state.jobs.push_back(*job);
                }
            }
            Next::Wait { until } => self.handle_wait(outcome.task_id, outcome.provider, until)?,
            Next::RetryAfter {
                until,
                attempt,
                detail,
            } => {
                self.handle_retry_after(outcome.task_id, outcome.provider, until, attempt, detail)?
            }
            Next::Halt(why) => {
                let state = self.deps.store.get_task(&outcome.task_id)?.state;
                self.record_halt(&outcome.task_id, why);
                self.requeue_if_ready(outcome.task_id, state);
            }
            Next::Blocked => {
                self.state.transient_retries.remove(&outcome.task_id);
                self.summary.blocked.push(outcome.task_id);
            }
            Next::DualReviewPartial { .. } => {}
        }
        Ok(())
    }

    pub(super) fn handle_gate(
        &mut self,
        job: GateJob,
        resolution: Result<Resolution, String>,
    ) -> PortResult<()> {
        self.state.in_gate -= 1;
        let stopping = self.is_stopping();
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
                if stopping {
                    self.clear_gating(&job.task_id);
                } else {
                    self.state.jobs.push_back(Job::Audit { job, lock_fp });
                }
            }
            Ok(Resolution::NeedsPostAudit { receipt }) => {
                let pre_audit = self.state.pre_audits.remove(&job.task_id);
                if stopping {
                    self.clear_gating(&job.task_id);
                } else if self.deps.settings.repo.cross_audit.dual_review {
                    // §D.4.1: PreGateAudit never bypasses Dual-Review.
                    self.start_dual_review(job, receipt)?;
                } else {
                    self.state.jobs.push_back(Job::PostAudit {
                        job,
                        receipt,
                        pre_audit,
                    });
                }
            }
            Ok(Resolution::Failed(signal)) => {
                self.state.pre_audits.remove(&job.task_id);
                if stopping {
                    self.clear_gating(&job.task_id);
                } else {
                    self.state.jobs.push_back(Job::Unblock { job, signal });
                }
            }
            Ok(Resolution::Retry) => {
                if stopping {
                    self.clear_gating(&job.task_id);
                } else {
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

    /// Blocks `task` on the scheduler thread when its root budget is exhausted
    /// before dispatching a Plan Debate turn (R3-CONC-4), preventing worker-level
    /// double-freeze races.
    pub(super) fn block_budget_exceeded(
        &mut self,
        task: &Task,
        budget: &RootBudget,
    ) -> PortResult<()> {
        let detail = format!(
            "root budget exceeded (tokens {}/{}, wall {}s/{}s, cycles {}/{})",
            budget.tokens_used,
            budget.token_limit,
            budget.wall_secs_used,
            budget.wall_secs_limit,
            budget.cycles_used,
            budget.cycles_limit,
        );
        if !budget.frozen {
            self.deps.store.set_frozen(&task.root_id, true)?;
            self.deps.emit(
                EventKind::BudgetFrozen,
                &task.root_id,
                json!({ "root_id": task.root_id, "why": detail }),
            )?;
        }
        let reason = BlockedReason::BudgetExceeded {
            detail: detail.clone(),
        };
        self.deps
            .move_to(&task.id, TaskState::Blocked, Some(&reason), &detail)?;
        unblock::ladder(self.deps).record_root_result(&task.root_id, false)?;
        self.summary.blocked.push(task.id.clone());
        Ok(())
    }

    /// Computes how long `Scheduler::drive` waits before waking an idle slot in
    /// `Mode::Auto`, bounded to `[200ms, 5s]`.
    pub(super) fn next_wake_duration(&self) -> Duration {
        const MIN_WAKE: Duration = Duration::from_millis(200);
        const MAX_WAKE: Duration = Duration::from_secs(5);
        let now = self.deps.now();
        let earliest = self
            .state
            .task_wait_until
            .values()
            .copied()
            .chain(self.state.default_wait_until.values().copied())
            .min();
        match earliest {
            Some(deadline) => {
                let secs = deadline.saturating_sub(now);
                if secs <= 0 {
                    MIN_WAKE
                } else {
                    Duration::from_secs(u64::try_from(secs).unwrap_or(5)).clamp(MIN_WAKE, MAX_WAKE)
                }
            }
            None => MAX_WAKE,
        }
    }
}

impl<'a> Scheduler<'a> {
    pub(super) fn try_start_board_candidate<'scope>(
        &mut self,
        scope: &'scope Scope<'scope, '_>,
        board: &mut TickBoard,
        exhausted: &mut HashSet<ProviderKey>,
    ) -> PortResult<bool>
    where
        'a: 'scope,
    {
        let mut idx = 0;
        while idx < board.candidates.len() {
            let overlaps = board
                .active_scopes
                .iter()
                .any(|s| schedule_order::scopes_overlap(&board.candidates[idx].scope_paths, s));
            if overlaps {
                idx += 1;
                continue;
            }
            let debate_step =
                plan_job::next_debate_step_for_task(self.deps, &board.candidates[idx])?;
            match debate_step {
                Some(DebateNext::Stalled { reason }) => {
                    let task = board.candidates.remove(idx);
                    unblock::block_untried(
                        self.deps,
                        &task.id,
                        &format!("{PLAN_DEBATE_STALLED_PREFIX} {reason}"),
                    )?;
                    self.summary.blocked.push(task.id);
                    continue;
                }
                Some(DebateNext::RunAuthor { round })
                | Some(DebateNext::RunChallenger { round }) => {
                    let role = if matches!(debate_step, Some(DebateNext::RunAuthor { .. })) {
                        DebateRole::Author
                    } else {
                        DebateRole::Challenger
                    };
                    let resolved = match self.router {
                        Some(r) => match role {
                            DebateRole::Author => r.resolve_plan_author(&self.deps.settings.repo),
                            DebateRole::Challenger => {
                                r.resolve_plan_challenger(&self.deps.settings.repo)
                            }
                        },
                        None => Err(PortError::Invalid(
                            "plan debate is enabled, but no StageRouter was configured".to_string(),
                        )),
                    };
                    let target = match resolved {
                        Ok(t) => t,
                        Err(err) => {
                            let task = board.candidates.remove(idx);
                            let out = worker::block_infra(self.deps, &task.id, &err.to_string());
                            self.summary.errors.extend(out.error);
                            self.summary.blocked.push(task.id);
                            continue;
                        }
                    };
                    if exhausted.contains(&target.provider_key) {
                        idx += 1;
                        continue;
                    }
                    let budget = worker::ensure_budget(self.deps, &board.candidates[idx].root_id)?;
                    if budget.check() != BudgetCheck::Ok {
                        let task = board.candidates.remove(idx);
                        self.block_budget_exceeded(&task, &budget)?;
                        if super::is_frozen(&*self.deps.store)? {
                            return Ok(false);
                        }
                        continue;
                    }
                    let Some(grant) = target
                        .accounts
                        .acquire_for(&board.candidates[idx].id, Purpose::Work)?
                    else {
                        exhausted.insert(target.provider_key.clone());
                        idx += 1;
                        continue;
                    };
                    let provider = target.provider_key.clone();
                    let task = board.candidates.remove(idx);
                    self.deps
                        .move_to(&task.id, TaskState::Running, None, "plan debate turn")?;
                    board.open_review += 1;
                    if !task.scope_paths.is_empty() {
                        board.active_scopes.push(task.scope_paths);
                    }
                    self.parked.remove(&task.id);
                    self.summary.requeued.retain(|id| id != &task.id);
                    self.spawn_worker(
                        scope,
                        Job::PlanTurn {
                            task_id: task.id,
                            role,
                            round,
                        },
                        provider,
                        grant,
                    );
                    return Ok(true);
                }
                Some(DebateNext::Converged { plan, .. }) => {
                    if self.try_start_agent(scope, board, idx, Some(&plan), exhausted)? {
                        return Ok(true);
                    }
                    idx += 1;
                }
                None => {
                    let latest = if self.router.is_some() {
                        plan_job::latest_converged_plan(self.deps, &board.candidates[idx].id)?
                            .map(|(p, _)| p)
                    } else {
                        None
                    };
                    if self.try_start_agent(scope, board, idx, latest.as_ref(), exhausted)? {
                        return Ok(true);
                    }
                    idx += 1;
                }
            }
        }
        Ok(false)
    }

    fn try_start_agent<'scope>(
        &mut self,
        scope: &'scope Scope<'scope, '_>,
        board: &mut TickBoard,
        idx: usize,
        plan: Option<&PlanContract>,
        exhausted: &mut HashSet<ProviderKey>,
    ) -> PortResult<bool>
    where
        'a: 'scope,
    {
        let task = &board.candidates[idx];
        let (provider, pool) = if let Some(router) = self.router {
            let (target, _, _) = router.resolve_implementer(task, plan, &self.deps.settings.repo);
            (target.provider_key.clone(), Arc::clone(&target.accounts))
        } else {
            (
                self.provider_key_for(false),
                Arc::clone(&self.deps.accounts),
            )
        };
        if exhausted.contains(&provider) {
            return Ok(false);
        }
        let Some(grant) = pool.acquire_for(&task.id, Purpose::Work)? else {
            exhausted.insert(provider);
            return Ok(false);
        };
        let task = board.candidates.remove(idx);
        self.deps
            .move_to(&task.id, TaskState::Running, None, "picked by aaf run")?;
        board.open_review += 1;
        if !task.scope_paths.is_empty() {
            board.active_scopes.push(task.scope_paths);
        }
        self.state.started.insert(task.id.clone());
        // The worker moves the task to `Gating` before its outcome is
        // handled: `poll_gating` must not queue a second gate for it.
        // `handle_worker` releases it for every non-gate outcome.
        self.gating_in_flight.insert(task.id.clone());
        self.parked.remove(&task.id);
        self.summary.requeued.retain(|id| id != &task.id);
        self.summary.agent_runs += 1;
        self.spawn_worker(scope, Job::Agent { task_id: task.id }, provider, grant);
        Ok(true)
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
