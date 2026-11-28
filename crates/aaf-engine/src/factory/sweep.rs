//! Recovery, review sweeps, live gating polls, and split-parent completion for
//! [`super::scheduler::Scheduler`].

use std::collections::HashSet;

use aaf_domain::event::EventKind;
use aaf_domain::ports::PortResult;
use aaf_domain::run::{RunFinish, RunStatus};
use aaf_domain::task::{Task, TaskState};
use serde_json::json;

use super::schedule_order::{MAX_TRANSIENT_RETRIES, transient_backoff_secs};
use super::scheduler::{CostGate, DEFAULT_RATE_LIMIT_COOLDOWN_SECS, Scheduler};
use super::stage_router::ProviderKey;
use super::worker::Job;
use super::{Deps, Mode, finish, unblock};
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

fn parse_kv_u64(text: &str, key: &str) -> u64 {
    text.split_whitespace()
        .find_map(|tok| tok.strip_prefix(key)?.parse::<u64>().ok())
        .unwrap_or(0)
}

fn recover_stream_usage(data_dir: &std::path::Path, run_id: &str) -> (u64, u64) {
    let path = data_dir.join("streams").join(format!("{run_id}.jsonl"));
    let Ok(text) = std::fs::read_to_string(&path) else {
        return (0, 0);
    };
    for line in text.lines().rev() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if matches!(v["type"].as_str(), Some("usage" | "finish")) {
            let target = v["target"].as_str().unwrap_or("");
            let tin = v["tokens_in"]
                .as_u64()
                .unwrap_or_else(|| parse_kv_u64(target, "tokens_in="));
            let tout = v["tokens_out"]
                .as_u64()
                .unwrap_or_else(|| parse_kv_u64(target, "tokens_out="));
            if tin > 0 || tout > 0 {
                return (tin, tout);
            }
        }
    }
    (0, 0)
}

pub(crate) fn reap_running_runs(
    deps: &Deps,
    task_id: &str,
    reap: &dyn Fn(&str) -> PortResult<()>,
) -> PortResult<()> {
    let root_id = deps.store.get_task(task_id).ok().map(|t| t.root_id);
    for run in deps.store.runs_for_task(task_id)? {
        if run.status != RunStatus::Running {
            continue;
        }
        reap(&run.id)?;
        let (tin, tout) = if run.tokens_in == 0 && run.tokens_out == 0 {
            recover_stream_usage(&deps.settings.data_dir, &run.id)
        } else {
            (run.tokens_in, run.tokens_out)
        };
        deps.store.finish_run(
            &run.id,
            &RunFinish {
                status: RunStatus::Killed,
                exit_code: None,
                tokens_in: tin,
                tokens_out: tout,
                ended_at: deps.now(),
            },
        )?;
        let total = tin.saturating_add(tout);
        if total > 0
            && let Some(rid) = root_id.as_deref()
        {
            let _ = deps.store.add_usage(rid, total, 0, 0);
        }
    }
    Ok(())
}

/// Puts back what an interrupted `aaf run` left behind: a task still
/// `Running` has no live run any more (its processes are reaped and it
/// returns to Ready); a task still `Gating` has any interrupted audit/unblock
/// runs reaped and only needs the gate again; a task still `Adopting` only has
/// its interrupted runs reaped and keeps its state.
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
    // An adopted PR keeps `Adopting`: its zombie reviewer/gate runs are reaped, but it is
    // neither sent back to Ready (the adopt lane re-checks the pinned head itself) nor
    // returned for a generic re-gate.
    for task in deps.store.list_tasks(repo, Some(TaskState::Adopting))? {
        reap_running_runs(deps, &task.id, reap)?;
    }
    Ok(gating.into_iter().map(|task| task.id).collect())
}

/// Cleans orphaned `aaf/batch-*` integration worktrees/branches, clean `Done`
/// task leftovers, dead-PID scratch worktrees/refs, and prunes stale git
/// worktree entries at scheduler startup without network, DB writes, or
/// `.run.lock`. Non-fatal warnings are returned rather than aborting startup.
pub(super) fn startup_sweep(deps: &Deps) -> Vec<String> {
    let mut warnings = Vec::new();
    let mut seen_raw = HashSet::new();
    let repo = Some(deps.settings.repo_root.as_str());
    let tasks = match deps.store.list_tasks(repo, None) {
        Ok(tasks) => tasks,
        Err(err) => {
            warnings.push(format!("startup_sweep list_tasks: {err}"));
            return warnings;
        }
    };
    let active_ids: HashSet<String> = tasks.iter().map(|task| task.id.clone()).collect();
    let done_tasks: Vec<(String, Option<String>)> = tasks
        .iter()
        .filter(|task| task.state == TaskState::Done)
        .map(|task| {
            let expected_head = deps
                .store
                .events_for_task_by_kinds(&task.id, &[EventKind::MergeResult], 10)
                .ok()
                .and_then(|events| crate::gc::verified_merged_head_from_events(&events));
            (task.id.clone(), expected_head)
        })
        .collect();
    match deps.git.sweep_orphan_batches(&active_ids) {
        Ok(refused) => {
            for w in refused {
                if seen_raw.insert(w.clone()) {
                    warnings.push(format!("startup_sweep orphan batches: {w}"));
                }
            }
        }
        Err(err) => warnings.push(format!("startup_sweep orphan batches: {err}")),
    }
    match deps
        .git
        .sweep_offline_done_with_heads(&done_tasks, &active_ids, None)
    {
        Ok(offline_warnings) => {
            for w in offline_warnings {
                if seen_raw.insert(w.clone()) {
                    warnings.push(format!("startup_sweep offline: {w}"));
                }
            }
        }
        Err(err) => warnings.push(format!("startup_sweep offline: {err}")),
    }
    if let Err(err) = deps.git.prune_worktrees() {
        warnings.push(format!("startup_sweep prune worktrees: {err}"));
    }
    warnings
}

impl<'a> Scheduler<'a> {
    pub(super) fn is_cooling_down(&self, provider: &ProviderKey) -> bool {
        self.state
            .default_wait_until
            .get(provider)
            .is_some_and(|&u| self.deps.now() < u)
    }

    pub(super) fn set_default_wait(&mut self, provider: ProviderKey, deadline: i64) {
        self.state
            .default_wait_until
            .entry(provider)
            .and_modify(|d| *d = (*d).max(deadline))
            .or_insert(deadline);
    }

    pub(super) fn clear_restarts_for_task(&mut self, task_id: &str) {
        self.state.restarts.retain(|(id, _), _| id != task_id);
    }

    /// Parks `task_id` after every account of `provider` was tried, with a
    /// deadline: without one, only an idle loop would ever un-park it, so a
    /// busy `Mode::Auto` daemon would starve it. `expire_waits` releases it
    /// (and its restart count) once the cooldown passes.
    pub(super) fn park_until_cooldown(&mut self, task_id: String, provider: ProviderKey) {
        let deadline = self.deps.now() + DEFAULT_RATE_LIMIT_COOLDOWN_SECS;
        self.state
            .task_wait_until
            .entry(task_id.clone())
            .and_modify(|d| *d = (*d).max(deadline))
            .or_insert(deadline);
        self.parked.insert(task_id, provider);
    }

    pub(super) fn handle_switch(
        &mut self,
        task_id: String,
        provider: ProviderKey,
    ) -> PortResult<()> {
        let state = self.deps.store.get_task(&task_id)?.state;
        let is_audit = state == TaskState::Gating;
        let count = self
            .pool_for_provider(&provider, is_audit)
            .provider_account_count()?;
        if count == 0 {
            let deadline = self.deps.now() + DEFAULT_RATE_LIMIT_COOLDOWN_SECS;
            self.set_default_wait(provider.clone(), deadline);
            self.state.task_wait_until.insert(task_id.clone(), deadline);
            self.parked.insert(task_id.clone(), provider);
            self.requeue_if_ready(task_id, state);
            return Ok(());
        }
        let restarts = self
            .state
            .restarts
            .entry((task_id.clone(), provider.clone()))
            .or_insert(0);
        let can_retry = *restarts < count.max(1);
        if can_retry {
            *restarts += 1;
        }
        if state == TaskState::Gating {
            if can_retry {
                self.queue_gating_task(task_id)?;
            } else {
                self.park_until_cooldown(task_id, provider);
            }
        } else {
            if can_retry {
                self.state.started.remove(&task_id);
            } else {
                self.park_until_cooldown(task_id.clone(), provider);
            }
            self.requeue_if_ready(task_id, state);
        }
        Ok(())
    }

    pub(super) fn handle_switch_job(
        &mut self,
        task_id: String,
        provider: ProviderKey,
        job: Job,
    ) -> PortResult<()> {
        let state = self.deps.store.get_task(&task_id)?.state;
        let is_audit = matches!(job, Job::Audit { .. } | Job::PostAudit { .. });
        let count = self
            .pool_for_provider(&provider, is_audit)
            .provider_account_count()?;
        if count == 0 {
            self.gating_in_flight.remove(&task_id);
            let deadline = self.deps.now() + DEFAULT_RATE_LIMIT_COOLDOWN_SECS;
            self.set_default_wait(provider.clone(), deadline);
            self.state.task_wait_until.insert(task_id.clone(), deadline);
            self.parked.insert(task_id.clone(), provider);
            self.requeue_if_ready(task_id, state);
            return Ok(());
        }
        let restarts = self
            .state
            .restarts
            .entry((task_id.clone(), provider.clone()))
            .or_insert(0);
        let can_retry = *restarts < count.max(1);
        if can_retry {
            *restarts += 1;
            self.state.jobs.push_back(job);
        } else {
            self.gating_in_flight.remove(&task_id);
            self.park_until_cooldown(task_id, provider);
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
            if let Some(provider) = self.parked.remove(&id) {
                self.state.restarts.remove(&(id, provider));
            } else {
                self.clear_restarts_for_task(&id);
            }
        }
        let expired_providers: HashSet<ProviderKey> = self
            .state
            .default_wait_until
            .iter()
            .filter(|(_, u)| **u <= now)
            .map(|(p, _)| p.clone())
            .collect();
        if !expired_providers.is_empty() {
            for provider in &expired_providers {
                self.state.default_wait_until.remove(provider);
            }
            let unpark: Vec<(String, ProviderKey)> = self
                .parked
                .iter()
                .filter(|(id, provider)| {
                    expired_providers.contains(*provider)
                        && !self
                            .state
                            .task_wait_until
                            .get(*id)
                            .is_some_and(|&u| u > now)
                })
                .map(|(id, p)| (id.clone(), p.clone()))
                .collect();
            for (id, provider) in unpark {
                self.parked.remove(&id);
                self.state.restarts.remove(&(id, provider));
            }
        }
    }

    pub(super) fn handle_wait(
        &mut self,
        task_id: String,
        provider: ProviderKey,
        until: Option<i64>,
    ) -> PortResult<()> {
        let state = self.deps.store.get_task(&task_id)?.state;
        let is_audit = state == TaskState::Gating;
        let now = self.deps.now();
        let deadline = until
            .filter(|&u| u > now)
            .unwrap_or(now + DEFAULT_RATE_LIMIT_COOLDOWN_SECS);
        let count = self
            .pool_for_provider(&provider, is_audit)
            .provider_account_count()?;
        self.state.task_wait_until.insert(task_id.clone(), deadline);
        if count == 0 {
            self.set_default_wait(provider.clone(), deadline);
        }
        self.parked.insert(task_id.clone(), provider);
        self.requeue_if_ready(task_id, state);
        Ok(())
    }

    pub(super) fn handle_retry_after(
        &mut self,
        task_id: String,
        provider: ProviderKey,
        hint_until: i64,
        hint_attempt: u32,
        detail: String,
        oauth_refreshed: bool,
    ) -> PortResult<()> {
        let state = self.deps.store.get_task(&task_id)?.state;
        self.requeue_if_ready(task_id.clone(), state);
        if oauth_refreshed {
            // Separate counter from network outage retries so a healed OAuth token
            // neither steals network budget nor gets blocked by an earlier blip.
            let already_tried = self.state.oauth_retries.get(&task_id).copied().unwrap_or(0);
            if already_tried == 0 {
                let can_dispatch = !self.is_stopping()
                    && self.summary.halted.is_none()
                    && !super::is_frozen(&*self.deps.store)?
                    && self.cost_gate()? == CostGate::Allow;
                if can_dispatch {
                    self.state.oauth_retries.insert(task_id.clone(), 1);
                    if state == TaskState::Gating {
                        self.queue_gating_task(task_id)?;
                    } else if self.mode == Mode::Once {
                        self.state.started.remove(&task_id);
                    }
                }
                return Ok(());
            }
            if matches!(self.mode, Mode::Once | Mode::Drain) {
                self.record_halt(&task_id, detail);
                return Ok(());
            }
            let tries = self.state.oauth_retries.entry(task_id.clone()).or_default();
            *tries = tries.saturating_add(1);
            let tries = *tries;
            let wait = if tries <= MAX_TRANSIENT_RETRIES + 1 {
                i64::try_from(transient_backoff_secs(tries - 1)).unwrap_or(60)
            } else {
                outage_wait_secs(tries - 1)
            };
            let deadline = hint_until.max(self.deps.now() + wait);
            self.state.task_wait_until.insert(task_id.clone(), deadline);
            if tries == MAX_TRANSIENT_RETRIES + 2 {
                self.notify_outage(&task_id, tries, wait, &detail)?;
            }
            return Ok(());
        }
        if matches!(self.mode, Mode::Once | Mode::Drain) {
            self.record_halt(&task_id, detail);
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
        self.set_default_wait(provider, deadline);
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
            if self.gating_in_flight.contains(&task.id)
                || self.parked.contains_key(&task.id)
                || waiting
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
        let is_split_parent = task.context_notes.iter().any(|n| {
            n.starts_with("split: spawned ")
                || (n.starts_with("unblock[T4]") && n.contains("split into "))
        });
        if task.depth != 0 || !is_split_parent {
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
        self.clear_restarts_for_task(&task.id);
        self.summary.done.push(task.id.clone());
        Ok(true)
    }
}
