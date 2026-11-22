//! The loop itself: the scheduler on the caller's thread, one thread per job
//! that needs an account, and one gate thread for the repository.

use std::collections::{HashMap, HashSet, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::Scope;

use aaf_domain::ports::{PortError, PortResult};
use aaf_domain::task::{Task, TaskState};

use super::audit_backend::AuditBackend;
use super::finish::Resolution;
pub(super) use super::finish::{GateStats, judge_round};
use super::ports::{AccountPool, Purpose};
use super::schedule_order;
pub use super::schedule_order::{
    MAX_TRANSIENT_BACKOFF_SECS, MAX_TRANSIENT_RETRIES, transient_backoff_secs,
};
use super::sweep;
pub use super::sweep::recover;
use super::worker::{self, GateJob, Job, Next, Outcome};
use super::{Deps, Mode, RunSummary, is_frozen, unblock};

const MAX_GATE_RETRIES: u32 = 3;
const DEFAULT_RATE_LIMIT_COOLDOWN_SECS: i64 = 300;

/// What the caller supplies besides the ports.
pub struct Hooks<'a> {
    /// Called in [`Mode::Auto`] when nothing can run right now; returns `false` to stop.
    pub idle: &'a (dyn Fn() -> bool + Sync),
    /// Kills what is left of an interrupted run (`executors::reaper::reap`).
    pub reap: &'a (dyn Fn(&str) -> PortResult<()> + Sync),
}

enum Msg {
    Worker(Outcome),
    Gate {
        job: GateJob,
        resolution: Result<Resolution, String>,
    },
}

/// Runs the factory for `deps.settings` until idle ([`Mode::Once`]), stopped, or frozen.
pub fn run(deps: &Deps, mode: Mode, hooks: &Hooks<'_>) -> PortResult<RunSummary> {
    run_with_audit(deps, None, mode, hooks)
}

/// [`run`] with an optional separate [`AuditBackend`] for cross-provider audits.
pub fn run_with_audit(
    deps: &Deps,
    audit: Option<&AuditBackend>,
    mode: Mode,
    hooks: &Hooks<'_>,
) -> PortResult<RunSummary> {
    let mut summary = RunSummary::default();
    if is_frozen(&*deps.store)? {
        summary.frozen = true;
        return Ok(summary);
    }
    let resumed = recover(deps, hooks.reap)?;
    summary.errors.extend(sweep::startup_sweep(deps));
    let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
    let (gate_tx, gate_rx) = mpsc::channel::<GateJob>();
    let stats = GateStats::default();
    let result = std::thread::scope(|scope| {
        let (gate_msgs, stats) = (msg_tx.clone(), &stats);
        scope.spawn(move || gate_loop(deps, &gate_rx, &gate_msgs, stats));
        let mut scheduler = Scheduler {
            deps,
            audit,
            mode,
            msg_tx,
            msg_rx,
            gate_tx,
            parked: HashSet::new(),
            gating_in_flight: HashSet::new(),
            state: LoopState::default(),
            summary,
        };
        scheduler
            .drive(scope, hooks, resumed)
            .map(|()| scheduler.summary)
    });
    let mut summary = result?;
    summary.gate_runs = stats.runs.load(Ordering::Relaxed);
    if let Ok(mut warnings) = stats.warnings.lock() {
        summary.errors.append(&mut warnings);
    }
    Ok(summary)
}

#[derive(Default)]
struct LoopState {
    jobs: VecDeque<Job>,
    active: usize,
    in_gate: usize,
    started: HashSet<String>,
    restarts: HashMap<String, usize>,
    gate_retries: HashMap<String, u32>,
    transient_retries: HashMap<String, u32>,
    default_wait_until: Option<i64>,
    task_wait_until: HashMap<String, i64>,
}

pub(super) struct Scheduler<'a> {
    pub(super) deps: &'a Deps,
    audit: Option<&'a AuditBackend>,
    mode: Mode,
    msg_tx: Sender<Msg>,
    msg_rx: Receiver<Msg>,
    gate_tx: Sender<GateJob>,
    pub(super) parked: HashSet<String>,
    pub(super) gating_in_flight: HashSet<String>,
    state: LoopState,
    pub(super) summary: RunSummary,
}

impl<'a> Scheduler<'a> {
    fn drive<'scope>(
        &mut self,
        scope: &'scope Scope<'scope, '_>,
        hooks: &Hooks<'_>,
        resumed: Vec<String>,
    ) -> PortResult<()>
    where
        'a: 'scope,
    {
        let fresh = self.fetch_base();
        self.sweep_review(fresh)?;
        for task_id in resumed {
            self.queue_gating_task(task_id)?;
        }
        loop {
            let frozen = is_frozen(&*self.deps.store)?;
            let halted = self.summary.halted.is_some();
            if !frozen && !halted {
                if self.mode == Mode::Auto {
                    self.poll_gating()?;
                }
                self.start_jobs(scope)?;
            }
            if self.state.active > 0 || self.state.in_gate > 0 {
                let msg = self
                    .msg_rx
                    .recv()
                    .map_err(|_| PortError::Io("factory message channel closed".to_string()))?;
                match msg {
                    Msg::Worker(outcome) => self.handle_worker(outcome)?,
                    Msg::Gate { job, resolution } => self.handle_gate(job, resolution)?,
                }
                continue;
            }
            self.summary.frozen = frozen;
            let startable = self.startable()?;
            let capped = !startable.is_empty()
                && self
                    .review_cap()
                    .is_some_and(|cap| self.open_review_and_scopes().is_ok_and(|(o, _)| o >= cap));
            let parked = self
                .parked
                .iter()
                .filter(|id| !startable.iter().any(|t| t.id == **id))
                .count();
            let waiting = self.state.jobs.len() + parked + if capped { 0 } else { startable.len() };
            if !halted {
                self.summary.waiting_for_account = u32::try_from(waiting).unwrap_or(u32::MAX);
            }
            if frozen || halted || self.mode == Mode::Once || !(hooks.idle)() {
                return Ok(());
            }
            self.parked.clear();
            self.sweep_review(false)?;
        }
    }

    pub(super) fn default_cooling_down(&mut self) -> bool {
        if self
            .state
            .default_wait_until
            .is_some_and(|u| self.deps.now() < u)
        {
            return true;
        }
        self.state.default_wait_until = None;
        false
    }

    pub(super) fn queue_gating_task(&mut self, task_id: String) -> PortResult<()> {
        match self.deps.git.task_worktree(&task_id) {
            Ok(worktree) => self.queue_gate(GateJob {
                task_id,
                worktree,
                audited: false,
            }),
            Err(err) => {
                let outcome = worker::block_infra(self.deps, &task_id, &err.to_string());
                self.summary.errors.extend(outcome.error);
                self.summary.blocked.push(task_id);
                Ok(())
            }
        }
    }

    fn review_cap(&self) -> Option<usize> {
        (!self.deps.settings.auto_merge && self.deps.settings.repo.run.max_open_review > 0)
            .then_some(self.deps.settings.repo.run.max_open_review as usize)
    }

    fn open_review_and_scopes(&self) -> PortResult<(usize, Vec<Vec<String>>)> {
        let tasks = self
            .deps
            .store
            .list_tasks(Some(self.deps.settings.repo_root.as_str()), None)?;
        Ok(schedule_order::open_review_and_scopes(&tasks))
    }

    /// Ready tasks whose dependencies are Done, ordered by priority then critical path.
    fn startable(&mut self) -> PortResult<Vec<Task>> {
        let now = self.deps.now();
        self.state
            .task_wait_until
            .retain(|_, &mut until| until > now);
        let can_settle = self.summary.halted.is_none() && !is_frozen(&*self.deps.store)?;
        let repo = Some(self.deps.settings.repo_root.as_str());
        loop {
            let all_tasks = self.deps.store.list_tasks(repo, None)?;
            let all_deps = self.deps.store.all_deps()?;
            let mut ready: Vec<Task> = all_tasks
                .iter()
                .filter(|t| t.state == TaskState::Ready)
                .cloned()
                .collect();
            schedule_order::sort_ready_by_priority_and_critical_path(&mut ready, &all_deps);
            self.summary.blocked_upstream =
                schedule_order::blocked_upstream_tasks(&ready, &all_tasks, &all_deps);
            let states: HashMap<&str, TaskState> =
                all_tasks.iter().map(|t| (t.id.as_str(), t.state)).collect();
            let deps_map = schedule_order::deps_by_task(&all_deps);
            let dep_done = |d: &str| {
                states.get(d).copied().map_or_else(
                    || {
                        self.deps
                            .store
                            .get_task(d)
                            .is_ok_and(|t| t.state == TaskState::Done)
                    },
                    |s| s == TaskState::Done,
                )
            };
            let (mut out, mut settled_parent) = (Vec::new(), false);
            for task in ready {
                if (self.mode == Mode::Once && self.state.started.contains(&task.id))
                    || self.parked.contains(&task.id)
                    || self
                        .state
                        .task_wait_until
                        .get(&task.id)
                        .is_some_and(|&u| u > now)
                    || deps_map
                        .get(task.id.as_str())
                        .is_some_and(|ds| ds.iter().any(|d| !dep_done(d)))
                {
                    continue;
                }
                if can_settle && self.try_auto_complete_split_parent(&task)? {
                    settled_parent = true;
                    continue;
                }
                out.push(task);
            }
            if !settled_parent {
                return Ok(out);
            }
        }
    }

    fn pool_for(&self, for_audit: bool) -> &dyn AccountPool {
        match (for_audit, self.audit) {
            (true, Some(backend)) => &*backend.accounts,
            _ => &*self.deps.accounts,
        }
    }

    fn pick_next_agent(&mut self, swept_for_cap: &mut bool) -> PortResult<Option<String>> {
        let candidates = self.startable()?;
        if candidates.is_empty() {
            return Ok(None);
        }
        let (mut open, mut scopes) = self.open_review_and_scopes()?;
        if let Some(cap) = self.review_cap()
            && open >= cap
        {
            if !*swept_for_cap {
                *swept_for_cap = true;
                self.sweep_review(false)?;
                (open, scopes) = self.open_review_and_scopes()?;
            }
            if open >= cap {
                self.summary.waiting_for_review_cap =
                    self.summary.waiting_for_review_cap.saturating_add(1);
                return Ok(None);
            }
        }
        Ok(candidates
            .into_iter()
            .find(|t| {
                !scopes
                    .iter()
                    .any(|s| schedule_order::scopes_overlap(&t.scope_paths, s))
            })
            .map(|t| t.id))
    }

    fn start_jobs<'scope>(&mut self, scope: &'scope Scope<'scope, '_>) -> PortResult<()>
    where
        'a: 'scope,
    {
        if self.default_cooling_down() {
            return Ok(());
        }
        let limit = self.deps.settings.max_parallel.max(1) as usize;
        let mut swept_for_cap = false;
        while self.state.active < limit {
            let job = match self.state.jobs.pop_front() {
                Some(job) => job,
                None => match self.pick_next_agent(&mut swept_for_cap)? {
                    Some(task_id) => Job::Agent { task_id },
                    None => break,
                },
            };
            let is_audit = matches!(job, Job::Audit { .. });
            let purpose = if is_audit {
                Purpose::Audit
            } else {
                Purpose::Work
            };
            let Some(grant) = self
                .pool_for(is_audit)
                .acquire_for(job.task_id(), purpose)?
            else {
                if !matches!(job, Job::Agent { .. }) {
                    self.state.jobs.push_front(job);
                }
                break;
            };
            if let Job::Agent { task_id } = &job {
                self.deps
                    .move_to(task_id, TaskState::Running, None, "picked by aaf run")?;
                self.state.started.insert(task_id.clone());
                self.parked.remove(task_id);
                self.summary.requeued.retain(|id| id != task_id);
                self.summary.agent_runs += 1;
            }
            self.state.active += 1;
            let (deps, audit, tx) = (self.deps, self.audit, self.msg_tx.clone());
            scope.spawn(move || {
                let task_id = job.task_id().to_string();
                let outcome = catch_unwind(AssertUnwindSafe(|| {
                    worker::run_job(deps, audit, job, &grant)
                }))
                .unwrap_or_else(|_| worker::block_infra(deps, &task_id, "the worker panicked"));
                drop(grant);
                let _ = tx.send(Msg::Worker(outcome));
            });
        }
        Ok(())
    }

    fn queue_gate(&mut self, job: GateJob) -> PortResult<()> {
        let id = job.task_id.clone();
        self.gating_in_flight.insert(id.clone());
        self.state.in_gate += 1;
        if self.gate_tx.send(job).is_err() {
            self.gating_in_flight.remove(&id);
            self.state.in_gate -= 1;
            return Err(PortError::Io("the gate thread is gone".to_string()));
        }
        Ok(())
    }

    fn requeue_if_ready(&mut self, task_id: String, state: TaskState) {
        if state == TaskState::Ready && !self.summary.requeued.contains(&task_id) {
            self.summary.requeued.push(task_id);
        }
    }

    fn clear_gating(&mut self, task_id: &str) {
        self.gating_in_flight.remove(task_id);
        self.state.gate_retries.remove(task_id);
    }

    fn handle_worker(&mut self, outcome: Outcome) -> PortResult<()> {
        self.state.active -= 1;
        self.summary.errors.extend(outcome.error);
        if !matches!(outcome.next, Next::Gate(_) | Next::Unblock { .. }) {
            self.gating_in_flight.remove(&outcome.task_id);
        }
        match outcome.next {
            Next::Gate(job) => {
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
                },
                signal,
            }),
            Next::Ready => {
                self.state.transient_retries.remove(&outcome.task_id);
                self.summary.requeued.push(outcome.task_id);
            }
            Next::Switch => self.handle_switch(outcome.task_id)?,
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

    fn handle_switch(&mut self, task_id: String) -> PortResult<()> {
        let state = self.deps.store.get_task(&task_id)?.state;
        let count = self
            .pool_for(state == TaskState::Gating)
            .provider_account_count()?;
        if count == 0 {
            self.state.default_wait_until =
                Some(self.deps.now() + DEFAULT_RATE_LIMIT_COOLDOWN_SECS);
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

    fn handle_wait(&mut self, task_id: String, until: Option<i64>) -> PortResult<()> {
        let state = self.deps.store.get_task(&task_id)?.state;
        let now = self.deps.now();
        let count = self
            .pool_for(state == TaskState::Gating)
            .provider_account_count()?;
        if count == 0 {
            let deadline = until
                .filter(|&u| u > now)
                .unwrap_or(now + DEFAULT_RATE_LIMIT_COOLDOWN_SECS);
            self.state.default_wait_until = Some(deadline);
        } else if let Some(deadline) = until.filter(|&u| u > now) {
            self.state.task_wait_until.insert(task_id.clone(), deadline);
        }
        self.parked.insert(task_id.clone());
        self.requeue_if_ready(task_id, state);
        Ok(())
    }

    fn handle_retry_after(
        &mut self,
        task_id: String,
        hint_until: i64,
        hint_attempt: u32,
        detail: String,
    ) -> PortResult<()> {
        let state = self.deps.store.get_task(&task_id)?.state;
        self.requeue_if_ready(task_id.clone(), state);
        if self.mode == Mode::Once {
            self.summary.halted.get_or_insert(detail);
            return Ok(());
        }
        let tries = self
            .state
            .transient_retries
            .entry(task_id.clone())
            .or_insert(0);
        *tries = (*tries)
            .max(hint_attempt.saturating_sub(1))
            .saturating_add(1);
        if *tries <= MAX_TRANSIENT_RETRIES {
            let wait = i64::try_from(transient_backoff_secs(*tries)).unwrap_or(60);
            let deadline = hint_until.max(self.deps.now() + wait);
            self.state.task_wait_until.insert(task_id, deadline);
        } else {
            self.summary.halted.get_or_insert(detail);
        }
        Ok(())
    }

    fn handle_gate(
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
                self.state.jobs.push_back(Job::Audit { job, lock_fp });
            }
            Ok(Resolution::Failed(signal)) => {
                self.state.jobs.push_back(Job::Unblock { job, signal });
            }
            Ok(Resolution::Retry) => {
                let count = self
                    .state
                    .gate_retries
                    .entry(job.task_id.clone())
                    .or_insert(0);
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

fn gate_loop(deps: &Deps, queue: &Receiver<GateJob>, tx: &Sender<Msg>, stats: &GateStats) {
    while let Ok(first) = queue.recv() {
        let mut jobs = vec![first];
        jobs.extend(queue.try_iter());
        let pending = Mutex::new(jobs.clone());
        let round = catch_unwind(AssertUnwindSafe(|| {
            judge_round(deps, jobs, stats, &|job, resolution| {
                if let Ok(mut pending) = pending.lock() {
                    pending.retain(|p| p.task_id != job.task_id);
                }
                let _ = tx.send(Msg::Gate { job, resolution });
            });
        }));
        if round.is_err() {
            let left = pending.lock().map(|mut p| std::mem::take(&mut *p));
            for job in left.unwrap_or_default() {
                let _ = tx.send(Msg::Gate {
                    job,
                    resolution: Err("the gate thread panicked".to_string()),
                });
            }
        }
    }
}
