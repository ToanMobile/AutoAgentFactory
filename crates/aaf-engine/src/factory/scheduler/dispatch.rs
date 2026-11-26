//! Lane-separated worker dispatch (`Coding` vs `Aux`) so read-only cross-audits,
//! blind parallel dual-review siblings, and deterministic unblocks never stall
//! behind `max_parallel = 1` coding turns.

use std::collections::{HashSet, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::thread::Scope;

use aaf_domain::ports::PortResult;

use super::{Msg, Scheduler, next_runnable};
use crate::factory::ports::{AccountPool, Grant, Purpose};
use crate::factory::stage_router::ProviderKey;
use crate::factory::worker::{self, Job};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WorkerLane {
    Coding,
    Aux,
}

/// Provider/pool resolution for queued aux jobs in [`Scheduler::start_jobs`].
pub(super) enum JobDispatch {
    Ready {
        provider: ProviderKey,
        pool: Arc<dyn AccountPool>,
        purpose: Purpose,
    },
    /// Deterministic unblock (T2/T3/Freeze/Escalate) runs without pre-acquiring an
    /// account lease; if T4 proposer is reached it acquires lazily or defers.
    NoAccount {
        provider: ProviderKey,
        cooling: bool,
    },
    DropAndAbort {
        task_id: String,
        message: String,
        is_dual_sibling: bool,
    },
}

impl<'a> Scheduler<'a> {
    pub(super) fn coding_parallel_limit(&self) -> usize {
        self.deps.settings.max_parallel.max(1) as usize
    }

    pub(super) fn aux_parallel_limit(&self) -> usize {
        let coding = self.coding_parallel_limit();
        let reviewers = self.deps.settings.repo.cross_audit.reviewers.len();
        coding.saturating_mul(2).max(reviewers).max(4)
    }

    fn reviewer_dispatch(
        &self,
        task_id: &str,
        reviewer_id: &str,
        kind: &str,
        is_dual_sibling: bool,
    ) -> JobDispatch {
        match self.router.and_then(|r| r.reviewer(reviewer_id)) {
            Some(target) => JobDispatch::Ready {
                provider: target.provider_key.clone(),
                pool: Arc::clone(&target.accounts),
                purpose: Purpose::Audit,
            },
            None => JobDispatch::DropAndAbort {
                task_id: task_id.to_string(),
                message: format!(
                    "{kind} reviewer {reviewer_id:?} is not configured in StageRouter"
                ),
                is_dual_sibling,
            },
        }
    }

    pub(super) fn resolve_job_dispatch(
        &self,
        job: &Job,
        cooling_providers: &HashSet<ProviderKey>,
    ) -> JobDispatch {
        match job {
            Job::DualReviewTurn { job, reviewer, .. } => {
                self.reviewer_dispatch(&job.task_id, &reviewer.id, "dual review", true)
            }
            Job::AdjudicateTurn {
                job,
                pass_reviewer_id,
                ..
            } => self.reviewer_dispatch(&job.task_id, pass_reviewer_id, "adjudication", false),
            Job::Unblock {
                needs_account: false,
                ..
            } => {
                let provider = self.provider_key_for(false);
                let cooling = cooling_providers.contains(&provider);
                JobDispatch::NoAccount { provider, cooling }
            }
            _ => {
                let is_audit = matches!(job, Job::Audit { .. } | Job::PostAudit { .. });
                let provider = self.provider_key_for(is_audit);
                let pool = self.pool_for_provider(&provider, is_audit);
                let purpose = if is_audit {
                    Purpose::Audit
                } else {
                    Purpose::Work
                };
                JobDispatch::Ready {
                    provider,
                    pool,
                    purpose,
                }
            }
        }
    }

    pub(super) fn spawn_worker<'scope>(
        &mut self,
        scope: &'scope Scope<'scope, '_>,
        lane: WorkerLane,
        job: Job,
        provider: ProviderKey,
        grant: Option<Grant>,
        provider_cooling: bool,
    ) where
        'a: 'scope,
    {
        match lane {
            WorkerLane::Coding => self.state.coding_active += 1,
            WorkerLane::Aux => self.state.aux_active += 1,
        }
        *self
            .state
            .in_flight_tasks
            .entry(job.task_id().to_string())
            .or_insert(0) += 1;
        let (deps, audit, router, tx) = (self.deps, self.audit, self.router, self.msg_tx.clone());
        scope.spawn(move || {
            let task_id = job.task_id().to_string();
            let provider_key = provider.clone();
            let mut outcome = catch_unwind(AssertUnwindSafe(|| {
                worker::run_job(
                    deps,
                    audit,
                    router,
                    provider,
                    job,
                    grant.as_ref(),
                    provider_cooling,
                )
            }))
            .unwrap_or_else(|_| worker::block_infra(deps, &task_id, "the worker panicked"));
            outcome.provider = provider_key;
            drop(grant);
            let _ = tx.send(Msg::Worker(lane, outcome));
        });
    }

    pub(super) fn start_jobs<'scope>(&mut self, scope: &'scope Scope<'scope, '_>) -> PortResult<()>
    where
        'a: 'scope,
    {
        let coding_limit = self.coding_parallel_limit();
        let aux_limit = self.aux_parallel_limit();
        let now = self.deps.now();
        let cooling_providers: HashSet<ProviderKey> = self
            .state
            .default_wait_until
            .iter()
            .filter(|(_, deadline)| now < **deadline)
            .map(|(provider, _)| provider.clone())
            .collect();
        // Only providers that returned `None` for `Purpose::Work` (or are cooling)
        // belong here: `Purpose::Audit` is task-specific (non-author exclusion),
        // so an audit deferral must never poison other tasks or `Purpose::Work`.
        let mut exhausted_work = cooling_providers.clone();
        let mut deferred: VecDeque<Job> = VecDeque::new();
        while self.state.aux_active < aux_limit
            && let Some(job) = self.state.jobs.pop_front()
        {
            match self.resolve_job_dispatch(&job, &cooling_providers) {
                JobDispatch::Ready {
                    provider,
                    pool,
                    purpose,
                } => {
                    let blocked = match purpose {
                        Purpose::Work => exhausted_work.contains(&provider),
                        Purpose::Audit => cooling_providers.contains(&provider),
                    };
                    let grant = if blocked {
                        None
                    } else {
                        pool.acquire_for(job.task_id(), purpose)?
                    };
                    let Some(grant) = grant else {
                        if purpose == Purpose::Work {
                            exhausted_work.insert(provider);
                        }
                        deferred.push_back(job);
                        continue;
                    };
                    self.spawn_worker(scope, WorkerLane::Aux, job, provider, Some(grant), false);
                }
                JobDispatch::NoAccount { provider, cooling } => {
                    self.spawn_worker(scope, WorkerLane::Aux, job, provider, None, cooling);
                }
                JobDispatch::DropAndAbort {
                    task_id,
                    message,
                    is_dual_sibling,
                } => {
                    self.abort_missing_reviewer_target(
                        task_id,
                        &message,
                        is_dual_sibling,
                        &mut deferred,
                    )?;
                }
            }
        }
        let has_deferred_audit = deferred.iter().any(|j| {
            matches!(
                j,
                Job::Audit { .. }
                    | Job::PostAudit { .. }
                    | Job::DualReviewTurn { .. }
                    | Job::AdjudicateTurn { .. }
            )
        });
        // When gating tasks are in-flight (about to need Cross-Audit) or an audit
        // job was just deferred waiting for an account, cap concurrent new coding
        // turns to 1 (or 0 once a coding turn is already active) so Ready coding
        // tasks cannot steal every free account and starve tasks 1 step from Done.
        let eff_coding_limit = if has_deferred_audit {
            0
        } else if self.state.in_gate > 0 {
            coding_limit.min(1)
        } else {
            coding_limit
        };
        let (mut board_slot, mut swept_for_cap) = (None, false);
        while self.state.coding_active < eff_coding_limit {
            if board_slot.is_none() {
                board_slot = Some(self.load_tick_board()?);
            }
            let Some(board) = board_slot.as_mut() else {
                break;
            };
            if next_runnable(board).is_none() {
                if self.mode == super::Mode::Auto
                    && board.has_review_dep_waiter
                    && self.maybe_sweep_review(&mut swept_for_cap)?
                {
                    *board = self.load_tick_board()?;
                }
                if next_runnable(board).is_none() {
                    break;
                }
            }
            if let Some(cap) = self.review_cap()
                && board.open_review >= cap
            {
                if self.maybe_sweep_review(&mut swept_for_cap)? {
                    *board = self.load_tick_board()?;
                }
                if board.open_review >= cap {
                    self.summary.waiting_for_review_cap =
                        self.summary.waiting_for_review_cap.saturating_add(1);
                    break;
                }
            }
            if !self.try_start_board_candidate(scope, board, &mut exhausted_work)? {
                break;
            }
        }
        while let Some(job) = deferred.pop_back() {
            self.state.jobs.push_front(job);
        }
        Ok(())
    }
}
