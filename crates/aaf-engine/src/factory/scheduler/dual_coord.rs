//! Blind Parallel Dual-Review scheduler coordination, crash recovery, sibling
//! abort cleanup, and adjudication dispatch (`docs/PLAN_DUAL_FRONTIER.md` §D.4,
//! R3-CONC-1, R3-CONC-4, R3-PROTO-3).

use std::collections::VecDeque;

use aaf_domain::budget::BudgetCheck;
use aaf_domain::gate::Receipt;
use aaf_domain::pipeline::{ReviewerConfig, canonical_provider};
use aaf_domain::ports::{PortError, PortResult};
use aaf_domain::task::TaskState;
use aaf_fp::PartialAuditRecord;

use super::Scheduler;
use crate::factory::dual_audit::{
    self, DeferredSiblingOutcome, DualFinalizeResult, DualReviewCoord, PartialTurnResult,
};
use crate::factory::ports::Grant;
use crate::factory::stage_router::{ProviderKey, StageRouter};
use crate::factory::worker::{self, GateJob, Job, Next, Outcome};
use crate::factory::{Deps, plan_job};

fn missing_target_err(kind: &str, id: &str) -> PortError {
    PortError::Invalid(format!(
        "{kind} reviewer {id:?} is not configured in StageRouter"
    ))
}

/// Executes [`Job::DualReviewTurn`] or [`Job::AdjudicateTurn`] on a worker
/// thread, wrapping [`Next::Switch`] back into [`Next::SwitchJob`].
pub(crate) fn run_dual_job(
    deps: &Deps,
    router: Option<&StageRouter>,
    job: Job,
    grant: &Grant,
) -> PortResult<Next> {
    match job {
        Job::DualReviewTurn {
            job,
            receipt,
            reviewer,
            plan_hash,
        } => {
            let target = router
                .and_then(|r| r.reviewer(&reviewer.id))
                .ok_or_else(|| missing_target_err("dual review", &reviewer.id))?;
            match dual_audit::run_reviewer_turn(
                deps, target, &reviewer, &job, &receipt, &plan_hash, grant,
            )? {
                PartialTurnResult::Completed(rec) => {
                    let reviewer_id = rec.reviewer_id.clone();
                    let inconclusive = dual_audit::is_inconclusive_partial(&rec).then_some(rec);
                    Ok(Next::DualReviewPartial {
                        reviewer_id,
                        inconclusive,
                    })
                }
                PartialTurnResult::Deferred(Next::Switch) => {
                    Ok(Next::SwitchJob(Box::new(Job::DualReviewTurn {
                        job,
                        receipt,
                        reviewer,
                        plan_hash,
                    })))
                }
                PartialTurnResult::Deferred(next) => Ok(next),
            }
        }
        Job::AdjudicateTurn {
            job,
            receipt,
            pass_reviewer_id,
            findings,
            partials,
            plan_hash,
        } => {
            let target = router
                .and_then(|r| r.reviewer(&pass_reviewer_id))
                .ok_or_else(|| missing_target_err("adjudication", &pass_reviewer_id))?;
            match dual_audit::run_adjudicate_turn(
                deps, target, &job, &receipt, &plan_hash, &findings, &partials, grant,
            )? {
                Next::Switch => Ok(Next::SwitchJob(Box::new(Job::AdjudicateTurn {
                    job,
                    receipt,
                    pass_reviewer_id,
                    findings,
                    partials,
                    plan_hash,
                }))),
                other => Ok(other),
            }
        }
        _ => Err(PortError::Invalid("unexpected non-dual job".to_string())),
    }
}

fn synthetic_inconclusive_partial(
    coord: &DualReviewCoord,
    rev: &ReviewerConfig,
    now: i64,
) -> PartialAuditRecord {
    let repo_root = std::fs::canonicalize(&coord.job.worktree.path)
        .unwrap_or_else(|_| coord.job.worktree.path.clone())
        .to_string_lossy()
        .into_owned();
    PartialAuditRecord {
        repo_root,
        tree_fp: coord.receipt.tree_fp.clone(),
        base_sha: coord.receipt.base_sha.clone().unwrap_or_default(),
        config_hash: coord.receipt.config_hash.clone(),
        plan_contract_hash: coord.plan_hash.clone(),
        reviewer_id: rev.id.clone(),
        executor: rev.executor.clone(),
        canonical_provider: canonical_provider(&rev.executor),
        model: rev.model.clone(),
        effort: rev.effort.clone(),
        verdict: "INCONCLUSIVE: reviewer did not produce a valid partial".to_string(),
        p0: 0,
        p1: 0,
        p2: 0,
        mechanical_score: 0,
        llm_average: None,
        effective_score: 0,
        scores: Vec::new(),
        issues: Vec::new(),
        created_at: now,
    }
}

fn purge_dual_turns(queue: &mut VecDeque<Job>, task_id: &str) -> usize {
    let before = queue.len();
    queue.retain(|j| !matches!(j, Job::DualReviewTurn { job, .. } if job.task_id == task_id));
    before.saturating_sub(queue.len())
}

fn exhausted_sibling(count: usize, provider: ProviderKey) -> DeferredSiblingOutcome {
    if count == 0 {
        DeferredSiblingOutcome::Wait {
            until: None,
            provider,
        }
    } else {
        DeferredSiblingOutcome::Switch { provider }
    }
}

impl Scheduler<'_> {
    fn push_blocked_unique(&mut self, task_id: String) {
        if !self.summary.blocked.contains(&task_id) {
            self.summary.blocked.push(task_id);
        }
    }

    pub(super) fn block_dual_infra(&mut self, task_id: &str, error: &str) {
        self.clear_gating(task_id);
        let outcome = worker::block_infra(self.deps, task_id, error);
        self.summary.errors.extend(outcome.error);
        self.push_blocked_unique(task_id.to_string());
    }

    fn try_dual<T>(&mut self, task_id: &str, res: PortResult<T>) -> Option<T> {
        match res {
            Ok(val) => Some(val),
            Err(err) => {
                self.block_dual_infra(task_id, &err.to_string());
                None
            }
        }
    }

    fn check_dual_budget(&mut self, task_id: &str) -> bool {
        let Some(task) = self.try_dual(task_id, self.deps.store.get_task(task_id)) else {
            return false;
        };
        let Some(budget) = self.try_dual(task_id, worker::ensure_budget(self.deps, &task.root_id))
        else {
            return false;
        };
        if budget.check() != BudgetCheck::Ok {
            self.clear_gating(task_id);
            if let Err(err) = self.block_budget_exceeded(&task, &budget) {
                self.block_dual_infra(task_id, &err.to_string());
            }
            return false;
        }
        true
    }

    pub(super) fn abort_missing_reviewer_target(
        &mut self,
        task_id: String,
        message: &str,
        is_dual_sibling: bool,
        deferred: &mut VecDeque<Job>,
    ) -> PortResult<()> {
        if !is_dual_sibling {
            self.block_dual_infra(&task_id, message);
            return Ok(());
        }
        let removed =
            purge_dual_turns(&mut self.state.jobs, &task_id) + purge_dual_turns(deferred, &task_id);
        let outcome = worker::block_infra(self.deps, &task_id, message);
        self.summary.errors.extend(outcome.error);
        let Some(coord) = self.state.dual_review_in_flight.get_mut(&task_id) else {
            self.clear_gating(&task_id);
            self.push_blocked_unique(task_id);
            return Ok(());
        };
        coord.in_flight = coord.in_flight.saturating_sub(1 + removed);
        coord.aborted = true;
        coord.deferred_outcome = Some(dual_audit::merge_deferred_outcome(
            coord.deferred_outcome.take(),
            DeferredSiblingOutcome::Blocked,
        ));
        if coord.in_flight == 0
            && let Some(coord) = self.state.dual_review_in_flight.remove(&task_id)
        {
            self.clear_gating(&task_id);
            if let Some(def) = coord.deferred_outcome {
                self.apply_deferred_sibling_outcome(task_id, def);
            }
        }
        Ok(())
    }

    /// Starts Blind Parallel Dual-Review for a `Clear` gate receipt (R3-CONC-1 & R3-CONC-4).
    pub(super) fn start_dual_review(
        &mut self,
        job: GateJob,
        receipt: Box<Receipt>,
    ) -> PortResult<()> {
        let cfg = &self.deps.settings.repo.cross_audit;
        if let Err(why) =
            dual_audit::validate_dual_quorum(cfg, &self.deps.settings.repo.run.executor)
        {
            self.block_dual_infra(&job.task_id, &why);
            return Ok(());
        }
        if !self.check_dual_budget(&job.task_id) {
            return Ok(());
        }
        let Some(plan_opt) = self.try_dual(
            &job.task_id,
            plan_job::latest_converged_plan(self.deps, &job.task_id),
        ) else {
            return Ok(());
        };
        let plan_hash = plan_opt.map_or_else(String::new, |(_, hash)| hash);
        let Some((missing, existing_partials)) = self.try_dual(
            &job.task_id,
            dual_audit::missing_reviewers(self.deps, &job, &receipt, &plan_hash),
        ) else {
            return Ok(());
        };
        // R3-CONC-1: 0-missing-partials crash recovery aggregates immediately.
        if missing.is_empty() {
            return self.finish_dual_review(job, receipt, plan_hash, existing_partials, false);
        }
        let coord = DualReviewCoord {
            in_flight: missing.len(),
            aborted: false,
            deferred_outcome: None,
            inconclusive_partials: Vec::new(),
            job: job.clone(),
            receipt: receipt.clone(),
            plan_hash: plan_hash.clone(),
        };
        self.state
            .dual_review_in_flight
            .insert(job.task_id.clone(), coord);
        for reviewer in missing {
            self.state.jobs.push_back(Job::DualReviewTurn {
                job: job.clone(),
                receipt: receipt.clone(),
                reviewer,
                plan_hash: plan_hash.clone(),
            });
        }
        Ok(())
    }

    /// Handles completion of one blind sibling [`Job::DualReviewTurn`] (R3-CONC-1).
    pub(super) fn handle_dual_sibling_outcome(&mut self, outcome: Outcome) -> PortResult<()> {
        let (task_id, provider) = (outcome.task_id, outcome.provider);
        let Some(aborted_before) = self.state.dual_review_in_flight.get_mut(&task_id).map(|c| {
            c.in_flight = c.in_flight.saturating_sub(1);
            c.aborted
        }) else {
            return Ok(());
        };
        let non_partial = match outcome.next {
            Next::DualReviewPartial {
                reviewer_id,
                inconclusive,
            } if !reviewer_id.is_empty() => {
                if let Some(rec) = inconclusive
                    && let Some(coord) = self.state.dual_review_in_flight.get_mut(&task_id)
                {
                    coord
                        .inconclusive_partials
                        .retain(|p| p.reviewer_id != rec.reviewer_id);
                    coord.inconclusive_partials.push(*rec);
                }
                None
            }
            Next::DualReviewPartial { .. } => {
                let out = worker::block_infra(
                    self.deps,
                    &task_id,
                    "dual review partial returned empty reviewer_id",
                );
                self.summary.errors.extend(out.error);
                Some(DeferredSiblingOutcome::Blocked)
            }
            Next::SwitchJob(job) => match self
                .pool_for_provider(&provider, false)
                .provider_account_count()
            {
                Err(err) => {
                    self.block_dual_infra(&task_id, &err.to_string());
                    Some(DeferredSiblingOutcome::Blocked)
                }
                Ok(count) => {
                    if !aborted_before && !self.is_stopping() && count > 0 {
                        let restarts = self
                            .state
                            .restarts
                            .entry((task_id.clone(), provider.clone()))
                            .or_insert(0);
                        if *restarts < count {
                            *restarts += 1;
                            if let Some(coord) = self.state.dual_review_in_flight.get_mut(&task_id)
                            {
                                coord.in_flight += 1;
                            }
                            self.state.jobs.push_back(*job);
                            return Ok(());
                        }
                    }
                    Some(exhausted_sibling(count, provider))
                }
            },
            Next::Halt(why) => Some(DeferredSiblingOutcome::Halt(why)),
            Next::Blocked => Some(DeferredSiblingOutcome::Blocked),
            Next::RetryAfter {
                until,
                attempt,
                detail,
                oauth_refreshed,
            } => Some(DeferredSiblingOutcome::RetryAfter {
                until,
                attempt,
                detail,
                provider,
                oauth_refreshed,
            }),
            Next::Wait { until } => Some(DeferredSiblingOutcome::Wait { until, provider }),
            Next::Switch => match self
                .pool_for_provider(&provider, false)
                .provider_account_count()
            {
                Ok(count) => Some(exhausted_sibling(count, provider)),
                Err(err) => {
                    self.block_dual_infra(&task_id, &err.to_string());
                    Some(DeferredSiblingOutcome::Blocked)
                }
            },
            Next::Gate(_)
            | Next::GateAudited(..)
            | Next::Ready
            | Next::RetryAudit(_)
            | Next::Unblock { .. } => {
                let out = worker::block_infra(
                    self.deps,
                    &task_id,
                    "unexpected Next variant from dual review sibling",
                );
                self.summary.errors.extend(out.error);
                Some(DeferredSiblingOutcome::Blocked)
            }
        };
        if let Some(incoming) = non_partial {
            let removed = purge_dual_turns(&mut self.state.jobs, &task_id);
            if let Some(coord) = self.state.dual_review_in_flight.get_mut(&task_id) {
                coord.in_flight = coord.in_flight.saturating_sub(removed);
                coord.aborted = true;
                coord.deferred_outcome = Some(dual_audit::merge_deferred_outcome(
                    coord.deferred_outcome.take(),
                    incoming,
                ));
            }
        }
        if !self
            .state
            .dual_review_in_flight
            .get(&task_id)
            .is_some_and(|c| c.in_flight == 0)
        {
            return Ok(());
        }
        let Some(coord) = self.state.dual_review_in_flight.remove(&task_id) else {
            return Ok(());
        };
        let Some(task) = self.try_dual(&task_id, self.deps.store.get_task(&task_id)) else {
            return Ok(());
        };
        if coord.aborted
            || self.summary.halted.is_some()
            || self.is_stopping()
            || task.state != TaskState::Gating
        {
            self.clear_gating(&task_id);
            if let Some(deferred) = coord.deferred_outcome {
                self.apply_deferred_sibling_outcome(task_id, deferred);
            }
            return Ok(());
        }
        let Some((still_missing, mut partials)) = self.try_dual(
            &task_id,
            dual_audit::missing_reviewers(self.deps, &coord.job, &coord.receipt, &coord.plan_hash),
        ) else {
            return Ok(());
        };
        let now = self.deps.now();
        for rev in &still_missing {
            if let Some(rec) = coord
                .inconclusive_partials
                .iter()
                .find(|p| p.reviewer_id == rev.id)
            {
                partials.push(rec.clone());
            } else {
                partials.push(synthetic_inconclusive_partial(&coord, rev, now));
            }
        }
        self.finish_dual_review(coord.job, coord.receipt, coord.plan_hash, partials, true)
    }

    fn apply_deferred_sibling_outcome(
        &mut self,
        task_id: String,
        deferred: DeferredSiblingOutcome,
    ) {
        match deferred {
            DeferredSiblingOutcome::Halt(why) => {
                self.record_halt(&task_id, why);
                if let Some(task) = self.try_dual(&task_id, self.deps.store.get_task(&task_id)) {
                    self.requeue_if_ready(task_id.clone(), task.state);
                    if task.state == TaskState::Blocked {
                        self.push_blocked_unique(task_id);
                    }
                }
            }
            DeferredSiblingOutcome::Blocked => {
                self.state.clear_retry_counters(&task_id);
                self.push_blocked_unique(task_id);
            }
            DeferredSiblingOutcome::RetryAfter {
                until,
                attempt,
                detail,
                provider,
                oauth_refreshed,
            } => {
                let res = self.handle_retry_after(
                    task_id.clone(),
                    provider,
                    until,
                    attempt,
                    detail,
                    oauth_refreshed,
                );
                self.try_dual(&task_id, res);
            }
            DeferredSiblingOutcome::Wait { until, provider } => {
                let res = self.handle_wait(task_id.clone(), provider, until);
                self.try_dual(&task_id, res);
            }
            DeferredSiblingOutcome::Switch { provider } => {
                self.park_until_cooldown(task_id, provider);
            }
        }
    }

    fn finish_dual_review(
        &mut self,
        job: GateJob,
        receipt: Box<Receipt>,
        plan_hash: String,
        partials: Vec<PartialAuditRecord>,
        made_progress: bool,
    ) -> PortResult<()> {
        let task_id = job.task_id.clone();
        let Some(result) = self.try_dual(
            &task_id,
            dual_audit::finalize_dual_review(self.deps, &job, &receipt, &partials),
        ) else {
            return Ok(());
        };
        match result {
            DualFinalizeResult::Done(Next::Gate(gate_job)) => {
                self.state.clear_retry_counters(&task_id);
                if let Err(err) = self.queue_gate(gate_job) {
                    self.block_dual_infra(&task_id, &err.to_string());
                }
            }
            DualFinalizeResult::Done(Next::Ready) => {
                self.clear_gating(&task_id);
                self.clear_restarts_for_task(&task_id);
                self.state.clear_retry_counters(&task_id);
                self.summary.requeued.push(task_id);
            }
            DualFinalizeResult::Done(Next::Blocked) => {
                self.clear_gating(&task_id);
                self.state.clear_retry_counters(&task_id);
                self.push_blocked_unique(task_id);
            }
            DualFinalizeResult::Done(Next::Unblock {
                task_id: uid,
                worktree,
                signal,
            }) => {
                self.state.clear_retry_counters(&uid);
                self.state.jobs.push_front(Job::Unblock {
                    job: GateJob {
                        task_id: uid,
                        worktree,
                        audited: false,
                        cached_receipt: None,
                    },
                    signal,
                    needs_account: false,
                });
            }
            DualFinalizeResult::Done(_) => {
                self.block_dual_infra(
                    &task_id,
                    "unexpected Next variant from finalize_dual_review",
                );
            }
            DualFinalizeResult::NeedsAdjudication {
                pass_reviewer_id,
                findings,
                partials,
            } => {
                // Only reset retry counters after sibling reviewers actually
                // made progress in this round; never wipe counters on the
                // `missing.is_empty()` re-entry path after `AdjudicateTurn` failed.
                if made_progress {
                    self.state.clear_retry_counters(&task_id);
                }
                if !self.check_dual_budget(&task_id) {
                    return Ok(());
                }
                self.state.jobs.push_back(Job::AdjudicateTurn {
                    job,
                    receipt,
                    pass_reviewer_id,
                    findings,
                    partials,
                    plan_hash,
                });
            }
        }
        Ok(())
    }
}
