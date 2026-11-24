//! The loop itself: the scheduler on the caller's thread, one thread per job
//! that needs an account, and one gate thread for the repository.

mod outcome;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;

use std::collections::{HashMap, HashSet, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::Scope;
use std::time::Instant;

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
use super::worker::{self, GateJob, Job, Outcome, PreGateAudit};
use super::{Deps, Mode, RunSummary, is_frozen};
use outcome::gate_loop;

const MAX_GATE_RETRIES: u32 = 3;
pub(super) const DEFAULT_RATE_LIMIT_COOLDOWN_SECS: i64 = 300;

/// What the caller supplies besides the ports.
pub struct Hooks<'a> {
    /// Called in [`Mode::Auto`] when nothing can run right now; returns `false` to stop.
    pub idle: &'a (dyn Fn() -> bool + Sync),
    /// Kills what is left of an interrupted run (`executors::reaper::reap`).
    pub reap: &'a (dyn Fn(&str) -> PortResult<()> + Sync),
}

enum Msg {
    Worker(Outcome),
    Gate(GateJob, Result<Resolution, String>),
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
    let (gate_tx, gate_rx) = mpsc::channel::<Vec<GateJob>>();
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
pub(super) struct LoopState {
    pub(super) jobs: VecDeque<Job>,
    active: usize,
    in_gate: usize,
    pub(super) started: HashSet<String>,
    pub(super) restarts: HashMap<String, usize>,
    gate_retries: HashMap<String, u32>,
    pub(super) transient_retries: HashMap<String, u32>,
    pub(super) work_default_wait_until: Option<i64>,
    pub(super) audit_default_wait_until: Option<i64>,
    pub(super) task_wait_until: HashMap<String, i64>,
    pub(super) last_review_sweep: Option<Instant>,
    pub(super) last_review_sweep_now: Option<i64>,
    pre_audits: HashMap<String, Box<PreGateAudit>>,
}

struct TickBoard {
    candidates: Vec<Task>,
    open_review: usize,
    active_scopes: Vec<Vec<String>>,
    has_review_dep_waiter: bool,
}

fn next_runnable(board: &TickBoard) -> Option<usize> {
    board.candidates.iter().position(|t| {
        !board
            .active_scopes
            .iter()
            .any(|s| schedule_order::scopes_overlap(&t.scope_paths, s))
    })
}

pub(super) struct Scheduler<'a> {
    pub(super) deps: &'a Deps,
    audit: Option<&'a AuditBackend>,
    pub(super) mode: Mode,
    msg_tx: Sender<Msg>,
    msg_rx: Receiver<Msg>,
    /// One message is one set of jobs the gate thread takes together.
    gate_tx: Sender<Vec<GateJob>>,
    pub(super) parked: HashSet<String>,
    pub(super) gating_in_flight: HashSet<String>,
    pub(super) state: LoopState,
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
        self.state.last_review_sweep = Some(Instant::now());
        self.state.last_review_sweep_now = Some(self.deps.now());
        self.queue_gating_tasks(resumed)?;
        let closed = || PortError::Io("factory message channel closed".to_string());
        loop {
            // A deadline can pass while the loop waits for any message, not
            // only on the idle-slot timeout: un-park its task either way.
            self.expire_waits();
            let frozen = is_frozen(&*self.deps.store)?;
            let halted = self.summary.halted.is_some();
            if !frozen && !halted {
                if matches!(self.mode, Mode::Auto | Mode::Drain) {
                    self.poll_gating()?;
                }
                self.start_jobs(scope)?;
            }
            let limit = self.deps.settings.max_parallel.max(1) as usize;
            if self.state.active > 0 || self.state.in_gate > 0 {
                let msg =
                    if self.mode == Mode::Auto && !frozen && !halted && self.state.active < limit {
                        match self.msg_rx.recv_timeout(self.next_wake_duration()) {
                            Ok(msg) => Some(msg),
                            Err(mpsc::RecvTimeoutError::Timeout) => None,
                            Err(mpsc::RecvTimeoutError::Disconnected) => return Err(closed()),
                        }
                    } else {
                        Some(self.msg_rx.recv().map_err(|_| closed())?)
                    };
                match msg {
                    Some(Msg::Worker(outcome)) => {
                        if matches!(
                            outcome.next,
                            worker::Next::Gate(_)
                                | worker::Next::GateAudited(..)
                                | worker::Next::Unblock { .. }
                                | worker::Next::Ready
                                | worker::Next::Blocked
                        ) {
                            self.state.restarts.remove(&outcome.task_id);
                        }
                        self.handle_worker(outcome)?;
                    }
                    Some(Msg::Gate(job, resolution)) => {
                        if matches!(resolution, Ok(Resolution::Settled(_))) {
                            self.state.restarts.remove(&job.task_id);
                        }
                        self.handle_gate(job, resolution)?;
                    }
                    None => {}
                }
                continue;
            }
            self.summary.frozen = frozen;
            let board = self.load_tick_board()?;
            let capped = !board.candidates.is_empty()
                && self
                    .review_cap()
                    .is_some_and(|cap| board.open_review >= cap);
            let parked = self
                .parked
                .iter()
                .filter(|id| !board.candidates.iter().any(|t| t.id == **id))
                .count();
            let waiting =
                self.state.jobs.len() + parked + if capped { 0 } else { board.candidates.len() };
            if !halted {
                self.summary.waiting_for_account = u32::try_from(waiting).unwrap_or(u32::MAX);
            }
            if frozen || halted || matches!(self.mode, Mode::Once | Mode::Drain) || !(hooks.idle)()
            {
                return Ok(());
            }
            for id in self.parked.drain() {
                self.state.restarts.remove(&id);
            }
            let _woken = self
                .deps
                .store
                .wake_due_blocked_tasks(&self.deps.settings.repo_root, self.deps.now())?;
            self.sweep_review(false)?;
            self.state.last_review_sweep = Some(Instant::now());
            self.state.last_review_sweep_now = Some(self.deps.now());
        }
    }

    pub(super) fn queue_gating_task(&mut self, task_id: String) -> PortResult<()> {
        self.queue_gating_tasks(vec![task_id])
    }

    /// Hands `Gating` tasks to the gate thread as one round, so they are
    /// batched together instead of split by when the gate thread wakes up.
    pub(super) fn queue_gating_tasks(&mut self, task_ids: Vec<String>) -> PortResult<()> {
        let mut jobs = Vec::with_capacity(task_ids.len());
        for task_id in task_ids {
            match self.deps.git.task_worktree(&task_id) {
                Ok(worktree) => jobs.push(GateJob {
                    task_id,
                    worktree,
                    audited: false,
                    cached_receipt: None,
                }),
                Err(err) => {
                    let outcome = worker::block_infra(self.deps, &task_id, &err.to_string());
                    self.summary.errors.extend(outcome.error);
                    self.summary.blocked.push(task_id);
                }
            }
        }
        self.queue_gates(jobs)
    }

    fn review_cap(&self) -> Option<usize> {
        (!self.deps.settings.effective_land().auto_lands()
            && self.deps.settings.repo.run.max_open_review > 0)
            .then_some(self.deps.settings.repo.run.max_open_review as usize)
    }

    /// Single-snapshot board view: ready candidates (deps Done, priority + critical
    /// path order), open-review count, and active (`Running | Gating`) scopes.
    fn load_tick_board(&mut self) -> PortResult<TickBoard> {
        self.expire_waits();
        let now = self.deps.now();
        let store = &*self.deps.store;
        let can_settle = self.summary.halted.is_none() && !is_frozen(store)?;
        let repo = Some(self.deps.settings.repo_root.as_str());
        loop {
            let all_tasks = store.list_tasks(repo, None)?;
            let all_deps = store.all_deps()?;
            let (open_review, active_scopes) = schedule_order::open_review_and_scopes(&all_tasks);
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
                    || store.get_task(d).is_ok_and(|t| t.state == TaskState::Done),
                    |s| s == TaskState::Done,
                )
            };
            let (mut candidates, mut settled_parent, mut has_review_dep_waiter) =
                (Vec::new(), false, false);
            for task in ready {
                let waiting = self
                    .state
                    .task_wait_until
                    .get(&task.id)
                    .is_some_and(|&u| u > now);
                if (self.mode == Mode::Once && self.state.started.contains(&task.id))
                    || self.parked.contains(&task.id)
                    || waiting
                {
                    continue;
                }
                if let Some(ds) = deps_map.get(task.id.as_str())
                    && ds.iter().any(|d| !dep_done(d))
                {
                    if ds
                        .iter()
                        .any(|&d| states.get(d).copied() == Some(TaskState::Review))
                    {
                        has_review_dep_waiter = true;
                    }
                    continue;
                }
                if can_settle && self.try_auto_complete_split_parent(&task)? {
                    settled_parent = true;
                    continue;
                }
                candidates.push(task);
            }
            if !settled_parent {
                return Ok(TickBoard {
                    candidates,
                    open_review,
                    active_scopes,
                    has_review_dep_waiter,
                });
            }
        }
    }

    pub(super) fn pool_for(&self, for_audit: bool) -> &dyn AccountPool {
        match (for_audit, self.audit) {
            (true, Some(backend)) => &*backend.accounts,
            _ => &*self.deps.accounts,
        }
    }

    fn pick_next_agent(
        &mut self,
        board_slot: &mut Option<TickBoard>,
        swept_for_cap: &mut bool,
    ) -> PortResult<Option<Task>> {
        if board_slot.is_none() {
            *board_slot = Some(self.load_tick_board()?);
        }
        let Some(board) = board_slot.as_mut() else {
            return Ok(None);
        };
        if next_runnable(board).is_none() {
            if self.mode == Mode::Auto
                && board.has_review_dep_waiter
                && self.maybe_sweep_review(swept_for_cap)?
            {
                *board = self.load_tick_board()?;
            }
            if next_runnable(board).is_none() {
                return Ok(None);
            }
        }
        if let Some(cap) = self.review_cap()
            && board.open_review >= cap
        {
            if self.maybe_sweep_review(swept_for_cap)? {
                *board = self.load_tick_board()?;
            }
            if board.open_review >= cap {
                self.summary.waiting_for_review_cap =
                    self.summary.waiting_for_review_cap.saturating_add(1);
                return Ok(None);
            }
        }
        Ok(next_runnable(board).map(|idx| board.candidates.remove(idx)))
    }

    fn start_jobs<'scope>(&mut self, scope: &'scope Scope<'scope, '_>) -> PortResult<()>
    where
        'a: 'scope,
    {
        let limit = self.deps.settings.max_parallel.max(1) as usize;
        let (mut board, mut swept_for_cap) = (None, false);
        let (mut work_exhausted, mut audit_exhausted) =
            (self.is_cooling_down(false), self.is_cooling_down(true));
        let mut deferred: VecDeque<Job> = VecDeque::new();
        while self.state.active < limit {
            let (job, picked_scopes) = match self.state.jobs.pop_front() {
                Some(job) => (job, None),
                None => {
                    if work_exhausted {
                        break;
                    }
                    match self.pick_next_agent(&mut board, &mut swept_for_cap)? {
                        Some(task) => (Job::Agent { task_id: task.id }, Some(task.scope_paths)),
                        None => break,
                    }
                }
            };
            let is_audit = matches!(job, Job::Audit { .. } | Job::PostAudit { .. });
            let (exhausted, purpose) = if is_audit {
                (&mut audit_exhausted, Purpose::Audit)
            } else {
                (&mut work_exhausted, Purpose::Work)
            };
            let grant = if *exhausted {
                None
            } else {
                self.pool_for(is_audit)
                    .acquire_for(job.task_id(), purpose)?
            };
            let Some(grant) = grant else {
                *exhausted = true;
                if !matches!(job, Job::Agent { .. }) {
                    deferred.push_back(job);
                }
                continue;
            };
            if let Job::Agent { task_id } = &job {
                self.deps
                    .move_to(task_id, TaskState::Running, None, "picked by aaf run")?;
                if let Some(ref mut snap) = board {
                    snap.open_review += 1;
                    if let Some(scopes) = picked_scopes
                        && !scopes.is_empty()
                    {
                        snap.active_scopes.push(scopes);
                    }
                }
                self.state.started.insert(task_id.clone());
                // The worker moves the task to `Gating` before its outcome is
                // handled: `poll_gating` must not queue a second gate for it.
                // `handle_worker` releases it for every non-gate outcome.
                self.gating_in_flight.insert(task_id.clone());
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
        while let Some(job) = deferred.pop_back() {
            self.state.jobs.push_front(job);
        }
        Ok(())
    }

    fn queue_gate(&mut self, job: GateJob) -> PortResult<()> {
        self.queue_gates(vec![job])
    }

    fn queue_gates(&mut self, jobs: Vec<GateJob>) -> PortResult<()> {
        if jobs.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = jobs.iter().map(|job| job.task_id.clone()).collect();
        self.gating_in_flight.extend(ids.iter().cloned());
        self.state.in_gate += ids.len();
        if self.gate_tx.send(jobs).is_err() {
            for id in &ids {
                self.gating_in_flight.remove(id);
            }
            self.state.in_gate -= ids.len();
            return Err(PortError::Io("the gate thread is gone".to_string()));
        }
        Ok(())
    }

    pub(super) fn requeue_if_ready(&mut self, task_id: String, state: TaskState) {
        if state == TaskState::Ready && !self.summary.requeued.contains(&task_id) {
            self.summary.requeued.push(task_id);
        }
    }
}
