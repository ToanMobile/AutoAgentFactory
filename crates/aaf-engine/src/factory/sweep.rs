//! Recovery, review sweeps, live gating polls, and split-parent completion for
//! [`super::scheduler::Scheduler`].

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use aaf_domain::event::EventKind;
use aaf_domain::ports::{PortError, PortResult};
use aaf_domain::run::{RunFinish, RunStatus};
use aaf_domain::task::{Task, TaskState};
use serde_json::json;

use super::schedule_order::{MAX_TRANSIENT_RETRIES, transient_backoff_secs};
use super::scheduler::{DEFAULT_RATE_LIMIT_COOLDOWN_SECS, Scheduler};
use super::worker::Job;
use super::{Deps, Mode, finish, unblock};
use crate::git::{GitError, TaskWorktree};
use crate::webhook::WebhookEvent;

/// First pause after the quick retries of a network failure; it doubles
/// with every further failure up to [`OUTAGE_WAIT_MAX_SECS`].
const OUTAGE_WAIT_SECS: i64 = 300;
const OUTAGE_WAIT_MAX_SECS: i64 = 1800;

/// `min(300 * 2^(tries - MAX_TRANSIENT_RETRIES - 1), 1800)` seconds.
fn outage_wait_secs(tries: u32) -> i64 {
    let doublings = tries.saturating_sub(MAX_TRANSIENT_RETRIES + 1).min(8);
    (OUTAGE_WAIT_SECS << doublings).min(OUTAGE_WAIT_MAX_SECS)
}

fn reap_running_runs(
    deps: &Deps,
    task_id: &str,
    reap: &dyn Fn(&str) -> PortResult<()>,
) -> PortResult<()> {
    for run in deps.store.runs_for_task(task_id)? {
        if run.status != RunStatus::Running {
            continue;
        }
        reap(&run.id)?;
        deps.store.finish_run(
            &run.id,
            &RunFinish {
                status: RunStatus::Killed,
                exit_code: None,
                tokens_in: run.tokens_in,
                tokens_out: run.tokens_out,
                ended_at: deps.now(),
            },
        )?;
    }
    Ok(())
}

/// Puts back what an interrupted `aaf run` left behind: a task still
/// `Running` has no live run any more (its processes are reaped and it
/// returns to Ready); a task still `Gating` has any interrupted audit/unblock
/// runs reaped and only needs the gate again.
pub fn recover(deps: &Deps, reap: &dyn Fn(&str) -> PortResult<()>) -> PortResult<Vec<String>> {
    let repo = Some(deps.settings.repo_root.as_str());
    for task in deps.store.list_tasks(repo, Some(TaskState::Running))? {
        reap_running_runs(deps, &task.id, reap)?;
        deps.move_to(
            &task.id,
            TaskState::Ready,
            None,
            "left running by an interrupted run",
        )?;
    }
    let gating = deps.store.list_tasks(repo, Some(TaskState::Gating))?;
    for task in &gating {
        reap_running_runs(deps, &task.id, reap)?;
    }
    Ok(gating.into_iter().map(|task| task.id).collect())
}

/// Cleans orphaned `aaf/batch-*` integration worktrees/branches and prunes
/// stale git worktree entries at scheduler startup. Non-fatal warnings are
/// returned rather than aborting startup.
pub(super) fn startup_sweep(deps: &Deps) -> Vec<String> {
    let mut warnings = Vec::new();
    let repo = Some(deps.settings.repo_root.as_str());
    let active_ids: HashSet<String> = match deps.store.list_tasks(repo, None) {
        Ok(tasks) => tasks.into_iter().map(|task| task.id).collect(),
        Err(err) => {
            warnings.push(format!("startup_sweep list_tasks: {err}"));
            return warnings;
        }
    };
    match deps.git.sweep_orphan_batches(&active_ids) {
        Ok(refused) => warnings.extend(
            refused
                .into_iter()
                .map(|w| format!("startup_sweep orphan batches: {w}")),
        ),
        Err(err) => warnings.push(format!("startup_sweep orphan batches: {err}")),
    }
    if let Err(err) = deps.git.prune_worktrees() {
        warnings.push(format!("startup_sweep prune worktrees: {err}"));
    }
    warnings
}

impl<'a> Scheduler<'a> {
    pub(super) fn is_cooling_down(&self, for_audit: bool) -> bool {
        let slot = if for_audit {
            self.state.audit_default_wait_until
        } else {
            self.state.work_default_wait_until
        };
        slot.is_some_and(|u| self.deps.now() < u)
    }

    pub(super) fn set_default_wait(&mut self, for_audit: bool, deadline: i64) {
        let slot = if for_audit {
            &mut self.state.audit_default_wait_until
        } else {
            &mut self.state.work_default_wait_until
        };
        *slot = Some(deadline);
    }

    pub(super) fn handle_switch(&mut self, task_id: String) -> PortResult<()> {
        let state = self.deps.store.get_task(&task_id)?.state;
        let is_audit = state == TaskState::Gating;
        let count = self.pool_for(is_audit).provider_account_count()?;
        if count == 0 {
            self.set_default_wait(is_audit, self.deps.now() + DEFAULT_RATE_LIMIT_COOLDOWN_SECS);
            self.parked.insert(task_id.clone());
            self.requeue_if_ready(task_id, state);
            return Ok(());
        }
        let restarts = self.state.restarts.entry(task_id.clone()).or_insert(0);
        let can_retry = *restarts < count.max(1);
        if can_retry {
            *restarts += 1;
        }
        if state == TaskState::Gating {
            if can_retry {
                self.queue_gating_task(task_id)?;
            } else {
                self.parked.insert(task_id);
            }
        } else {
            if can_retry {
                self.state.started.remove(&task_id);
            }
            self.summary.requeued.push(task_id);
        }
        Ok(())
    }

    pub(super) fn handle_switch_job(&mut self, task_id: String, job: Job) -> PortResult<()> {
        let state = self.deps.store.get_task(&task_id)?.state;
        let is_audit = matches!(job, Job::Audit { .. } | Job::PostAudit { .. });
        let count = self.pool_for(is_audit).provider_account_count()?;
        if count == 0 {
            self.gating_in_flight.remove(&task_id);
            self.set_default_wait(is_audit, self.deps.now() + DEFAULT_RATE_LIMIT_COOLDOWN_SECS);
            self.parked.insert(task_id.clone());
            self.requeue_if_ready(task_id, state);
            return Ok(());
        }
        let restarts = self.state.restarts.entry(task_id.clone()).or_insert(0);
        let can_retry = *restarts < count.max(1);
        if can_retry {
            *restarts += 1;
            self.state.jobs.push_back(job);
        } else {
            self.gating_in_flight.remove(&task_id);
            self.parked.insert(task_id);
        }
        Ok(())
    }

    /// Releases tasks whose wait/cooldown deadlines have elapsed so idle slots
    /// in `Mode::Auto` can pick them up immediately while sibling workers run.
    pub(super) fn expire_waits(&mut self) {
        let now = self.deps.now();
        let expired: Vec<String> = self
            .state
            .task_wait_until
            .iter()
            .filter(|(_, u)| **u <= now)
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            self.state.task_wait_until.remove(&id);
            self.parked.remove(&id);
            self.state.restarts.remove(&id);
        }
        if self.state.work_default_wait_until.is_some_and(|u| u <= now) {
            self.state.work_default_wait_until = None;
            for id in self.parked.drain() {
                self.state.restarts.remove(&id);
            }
        }
        if self
            .state
            .audit_default_wait_until
            .is_some_and(|u| u <= now)
        {
            self.state.audit_default_wait_until = None;
            for id in self.parked.drain() {
                self.state.restarts.remove(&id);
            }
        }
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
            .chain(self.state.work_default_wait_until)
            .chain(self.state.audit_default_wait_until)
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

    pub(super) fn handle_wait(&mut self, task_id: String, until: Option<i64>) -> PortResult<()> {
        let state = self.deps.store.get_task(&task_id)?.state;
        let is_audit = state == TaskState::Gating;
        let now = self.deps.now();
        let deadline = until
            .filter(|&u| u > now)
            .unwrap_or(now + DEFAULT_RATE_LIMIT_COOLDOWN_SECS);
        let count = self.pool_for(is_audit).provider_account_count()?;
        if count == 0 {
            self.set_default_wait(is_audit, deadline);
        } else {
            self.state.task_wait_until.insert(task_id.clone(), deadline);
        }
        self.parked.insert(task_id.clone());
        self.requeue_if_ready(task_id, state);
        Ok(())
    }

    pub(super) fn handle_retry_after(
        &mut self,
        task_id: String,
        hint_until: i64,
        hint_attempt: u32,
        detail: String,
    ) -> PortResult<()> {
        let state = self.deps.store.get_task(&task_id)?.state;
        self.requeue_if_ready(task_id.clone(), state);
        if matches!(self.mode, Mode::Once | Mode::Drain) {
            self.summary.halted.get_or_insert(detail);
            return Ok(());
        }
        let tries = self
            .state
            .transient_retries
            .entry(task_id.clone())
            .or_default();
        *tries = (*tries)
            .max(hint_attempt.saturating_sub(1))
            .saturating_add(1);
        let tries = *tries;
        if tries <= MAX_TRANSIENT_RETRIES {
            let wait = i64::try_from(transient_backoff_secs(tries)).unwrap_or(60);
            let deadline = hint_until.max(self.deps.now() + wait);
            self.state.task_wait_until.insert(task_id, deadline);
            return Ok(());
        }
        // O12-R10-1: an unattended daemon outlives an outage. The provider
        // is down for every task of this kind: all of them wait, longer each
        // time, and somebody is told once per outage.
        let wait = outage_wait_secs(tries);
        let deadline = hint_until.max(self.deps.now() + wait);
        self.set_default_wait(state == TaskState::Gating, deadline);
        self.state.task_wait_until.insert(task_id.clone(), deadline);
        if tries == MAX_TRANSIENT_RETRIES + 1 {
            self.notify_outage(&task_id, tries, wait, &detail)?;
        }
        Ok(())
    }

    fn notify_outage(&self, task_id: &str, tries: u32, wait: i64, detail: &str) -> PortResult<()> {
        let deps = self.deps;
        // The halt text's advice to start the run again does not apply here.
        let detail = detail.split(". The run stopped").next().unwrap_or(detail);
        let event = WebhookEvent {
            event_id: unblock::unique_id(),
            kind: EventKind::AccountCooldown,
            task_id: Some(task_id.to_string()),
            status: "network_wait".to_string(),
            summary: unblock::one_line(
                &format!(
                    "the provider could not be reached {tries} times in a row; the factory \
                     keeps running and tries again in {wait} s (waits grow up to \
                     {OUTAGE_WAIT_MAX_SECS} s). Last failure: {detail}"
                ),
                500,
            ),
            created_at: deps.now(),
        };
        // A failed notification is recorded; it never stops the loop.
        match deps.notifier.notify(&event) {
            Ok(true) => deps.emit(EventKind::WebhookSent, task_id, json!({ "ok": true })),
            Ok(false) => Ok(()),
            Err(e) => deps.emit(
                EventKind::WebhookSent,
                task_id,
                json!({ "ok": false, "error": e }),
            ),
        }
    }

    /// Picks up live `Gating` tasks (e.g. `aaf board rework` without `--rerun`)
    /// in `Mode::Auto` without restarting the daemon.
    pub(super) fn poll_gating(&mut self) -> PortResult<()> {
        if self.is_cooling_down(true) {
            return Ok(());
        }
        self.expire_waits();
        let now = self.deps.now();
        let repo = Some(self.deps.settings.repo_root.as_str());
        let mut queue = Vec::new();
        for task in self.deps.store.list_tasks(repo, Some(TaskState::Gating))? {
            let waiting = self
                .state
                .task_wait_until
                .get(&task.id)
                .is_some_and(|&u| u > now);
            if self.gating_in_flight.contains(&task.id) || self.parked.contains(&task.id) || waiting
            {
                continue;
            }
            queue.push(task.id);
        }
        self.queue_gating_tasks(queue)
    }

    /// Throttled review sweep shared by the review-cap and review-dep waiter paths.
    pub(super) fn maybe_sweep_review(&mut self, swept: &mut bool) -> PortResult<bool> {
        const INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
        let secs = INTERVAL.as_secs() as i64;
        let wall_due = self
            .state
            .last_review_sweep
            .is_none_or(|t| t.elapsed() >= INTERVAL);
        let clock_due = self
            .state
            .last_review_sweep_now
            .is_some_and(|t| self.deps.now().saturating_sub(t) >= secs);
        if *swept || !(wall_due || clock_due) {
            return Ok(false);
        }
        *swept = true;
        self.sweep_review(false)?;
        self.state.last_review_sweep = Some(std::time::Instant::now());
        self.state.last_review_sweep_now = Some(self.deps.now());
        Ok(true)
    }

    /// A task in Review whose branch has reached the base is done: this is
    /// how a merged pull request releases the tasks that depend on it.
    pub(super) fn sweep_review(&mut self, base_is_fresh: bool) -> PortResult<()> {
        let deps = self.deps;
        let repo = Some(deps.settings.repo_root.as_str());
        let review = deps.store.list_tasks(repo, Some(TaskState::Review))?;
        if review.is_empty() {
            return Ok(());
        }
        self.state.last_review_sweep = Some(std::time::Instant::now());
        // Offline: the tasks stay in Review and are looked at again on the
        // next sweep.
        if !(base_is_fresh || self.fetch_base()) {
            return Ok(());
        }
        let mut remaining: Vec<Task> = Vec::new();
        for task in review {
            if !deps.git.is_merged(&task.id)? {
                remaining.push(task);
                continue;
            }
            if deps.store.get_task(&task.id)?.state != TaskState::Review {
                continue;
            }
            match deps.move_to(
                &task.id,
                TaskState::Done,
                None,
                "branch is merged into the base",
            ) {
                Ok(()) => {}
                // A concurrent `aaf board rework` moved the task out of Review.
                Err(PortError::Invalid(_)) => continue,
                Err(err) => return Err(err),
            }
            finish::close(deps, &task)?;
            unblock::root_succeeded(deps, &task)?;
            if let Err(err) = deps.git.cleanup(&task.id) {
                self.summary.errors.push(format!("{}: {err}", task.id));
            }
            self.state.restarts.remove(&task.id);
            self.summary.done.push(task.id);
        }
        self.sweep_squash_merged(remaining)
    }

    fn sweep_squash_merged(&mut self, remaining: Vec<Task>) -> PortResult<()> {
        if remaining.is_empty() {
            return Ok(());
        }
        let deps = self.deps;
        let mut candidates: Vec<(String, String)> = Vec::new();
        let mut worktrees: HashMap<String, (Task, TaskWorktree)> = HashMap::new();
        for task in remaining {
            match deps.git.existing_worktree_head(&task.id) {
                Ok(Some((wt, head_oid))) => {
                    candidates.push((task.id.clone(), head_oid));
                    worktrees.insert(task.id.clone(), (task, wt));
                }
                Ok(None) => {}
                Err(err) => self.summary.errors.push(format!("{}: {err}", task.id)),
            }
        }
        if candidates.is_empty() {
            return Ok(());
        }
        let verified = match deps.git.verified_squash_merges(&candidates) {
            Ok(map) => map,
            Err(err) => {
                self.summary.errors.push(format!("squash_sweep: {err}"));
                return Ok(());
            }
        };
        for (task_id, _) in &candidates {
            let Some(expected_oid) = verified.get(task_id) else {
                continue;
            };
            let Some((task, wt)) = worktrees.remove(task_id) else {
                continue;
            };
            if deps.store.get_task(&task.id)?.state != TaskState::Review {
                continue;
            }
            match deps.git.remove_verified_merged(&wt, expected_oid) {
                Ok(()) => {}
                Err(GitError::Dirty(path)) => {
                    self.summary.errors.push(format!(
                        "{}: skipped squash-merge cleanup: uncommitted changes in {}",
                        task.id,
                        path.display()
                    ));
                    continue;
                }
                Err(err) => {
                    self.summary
                        .errors
                        .push(format!("{}: squash-merge cleanup failed: {err}", task.id));
                    continue;
                }
            }
            match deps.move_to(
                &task.id,
                TaskState::Done,
                None,
                "pull request squash-merged into the base",
            ) {
                Ok(()) => {}
                Err(PortError::Invalid(_)) => continue,
                Err(err) => return Err(err),
            }
            deps.emit(
                EventKind::MergeResult,
                &task.id,
                json!({
                    "merged": true,
                    "kind": "squash_pr",
                    "head_oid": expected_oid,
                }),
            )?;
            finish::close(deps, &task)?;
            unblock::root_succeeded(deps, &task)?;
            self.state.restarts.remove(&task.id);
            self.summary.done.push(task.id);
        }
        Ok(())
    }

    /// False when the remote could not be reached; the run goes on with the
    /// base it has.
    pub(super) fn fetch_base(&mut self) -> bool {
        match self.deps.git.fetch_base() {
            Ok(()) => true,
            Err(err) => {
                self.summary.errors.push(format!("fetch: {err}"));
                false
            }
        }
    }

    /// Auto-completes a proactive split container root whose subtasks are all
    /// `Done`, so the scheduler never spawns a redundant agent that fails with
    /// `NO_CHANGE`.
    pub(super) fn try_auto_complete_split_parent(&mut self, task: &Task) -> PortResult<bool> {
        if task.depth != 0
            || !task
                .context_notes
                .iter()
                .any(|n| n.starts_with("split: spawned "))
        {
            return Ok(false);
        }
        let children = self.deps.store.children_of(&task.id)?;
        if children.len() < 2 || !children.iter().all(|c| c.state == TaskState::Done) {
            return Ok(false);
        }
        self.deps.move_to(
            &task.id,
            TaskState::Running,
            None,
            "all split subtasks completed",
        )?;
        self.deps.move_to(
            &task.id,
            TaskState::Gating,
            None,
            "all split subtasks completed",
        )?;
        self.deps.move_to(
            &task.id,
            TaskState::Done,
            None,
            "all split subtasks completed",
        )?;
        finish::close(self.deps, task)?;
        unblock::root_succeeded(self.deps, task)?;
        self.state.restarts.remove(&task.id);
        self.summary.done.push(task.id.clone());
        Ok(true)
    }
}
