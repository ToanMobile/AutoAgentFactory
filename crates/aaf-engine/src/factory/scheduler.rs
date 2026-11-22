//! The loop itself: the scheduler on the caller's thread, one thread per job
//! that needs an account, and one gate thread for the repository.

use std::collections::{HashMap, HashSet, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::Scope;

use aaf_domain::gate::LockStatus;
use aaf_domain::ports::{PortError, PortResult};
use aaf_domain::run::{RunFinish, RunStatus};
use aaf_domain::task::TaskState;

use super::batch::{Member, gate_batch};
use super::finish::{self, Resolution, Round};
use super::ports::Purpose;
use super::worker::{self, GateJob, Job, Next, Outcome};
use super::{Deps, Mode, RunSummary, is_frozen};

/// What the caller supplies besides the ports.
pub struct Hooks<'a> {
    /// Called in [`Mode::Auto`] when nothing can run right now. Waits, then
    /// returns true to look again or false to stop the loop.
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

/// Written by the gate thread before it reports a task, read by the
/// scheduler when the loop ends.
#[derive(Default)]
pub(super) struct GateStats {
    pub(super) runs: AtomicU32,
    pub(super) warnings: Mutex<Vec<String>>,
}

/// Puts back what an interrupted `aaf run` left behind: a task still
/// `Running` has no live run any more (its processes are reaped and it
/// returns to Ready); a task still `Gating` only needs the gate again.
pub fn recover(deps: &Deps, reap: &dyn Fn(&str) -> PortResult<()>) -> PortResult<Vec<String>> {
    let repo = Some(deps.settings.repo_root.as_str());
    for task in deps.store.list_tasks(repo, Some(TaskState::Running))? {
        for run in deps.store.runs_for_task(&task.id)? {
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
        deps.move_to(
            &task.id,
            TaskState::Ready,
            None,
            "left running by an interrupted run",
        )?;
    }
    let gating = deps.store.list_tasks(repo, Some(TaskState::Gating))?;
    Ok(gating.into_iter().map(|task| task.id).collect())
}

/// Runs the factory for the repository of `deps.settings` until nothing is
/// left to do ([`Mode::Once`]), `hooks.idle` says stop, or the factory
/// freezes.
pub fn run(deps: &Deps, mode: Mode, hooks: &Hooks<'_>) -> PortResult<RunSummary> {
    let mut summary = RunSummary::default();
    if is_frozen(&*deps.store)? {
        summary.frozen = true;
        return Ok(summary);
    }
    let resumed = recover(deps, hooks.reap)?;
    let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
    let (gate_tx, gate_rx) = mpsc::channel::<GateJob>();
    let stats = GateStats::default();
    let result = std::thread::scope(|scope| {
        let gate_msgs = msg_tx.clone();
        let stats = &stats;
        scope.spawn(move || gate_loop(deps, &gate_rx, &gate_msgs, stats));
        let mut scheduler = Scheduler {
            deps,
            mode,
            msg_tx,
            msg_rx,
            gate_tx,
            jobs: VecDeque::new(),
            active: 0,
            in_gate: 0,
            started: HashSet::new(),
            restarts: HashMap::new(),
            parked: HashSet::new(),
            summary,
        };
        let result = scheduler.drive(scope, hooks, resumed);
        // Dropping the scheduler closes the gate queue, which ends the gate
        // thread; the scope then waits for every thread.
        result.map(|()| scheduler.summary)
    });
    let mut summary = result?;
    summary.gate_runs = stats.runs.load(Ordering::Relaxed);
    if let Ok(mut warnings) = stats.warnings.lock() {
        summary.errors.append(&mut warnings);
    }
    Ok(summary)
}

struct Scheduler<'a> {
    deps: &'a Deps,
    mode: Mode,
    msg_tx: Sender<Msg>,
    msg_rx: Receiver<Msg>,
    gate_tx: Sender<GateJob>,
    /// Audits and unblock steps waiting for an account; served before new
    /// tasks so work in flight finishes first.
    jobs: VecDeque<Job>,
    active: usize,
    in_gate: usize,
    /// Tasks whose agent ran in this invocation.
    started: HashSet<String>,
    /// Times a task was started again in this invocation after its account
    /// ran into a usage limit.
    restarts: HashMap<String, usize>,
    /// Tasks left Ready to wait for an account's cooldown.
    parked: HashSet<String>,
    summary: RunSummary,
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
        for task_id in resumed {
            let worktree = self.deps.git.task_worktree(&task_id)?;
            self.queue_gate(GateJob {
                task_id,
                worktree,
                audited: false,
            })?;
        }
        // New task branches are cut from the base as it is now, not as it
        // was at the last fetch.
        let fresh = self.fetch_base();
        self.sweep_review(fresh)?;
        loop {
            let frozen = is_frozen(&*self.deps.store)?;
            // After an infra failure no agent starts: work already running
            // and gates already queued finish, nothing new begins.
            let halted = self.summary.halted.is_some();
            if !frozen && !halted {
                self.start_jobs(scope)?;
            }
            if self.active > 0 || self.in_gate > 0 {
                // The scheduler holds a sender itself, so this only fails if
                // the channel is broken beyond repair.
                let msg = self
                    .msg_rx
                    .recv()
                    .map_err(|_| PortError::Io("factory message channel closed".to_string()))?;
                self.handle(msg)?;
                continue;
            }
            self.summary.frozen = frozen;
            // A parked task counts once: `startable` skips it in
            // `Mode::Once` (it was started) and lists it in `Mode::Auto`.
            let startable = self.startable()?;
            let parked = self.parked.iter().filter(|id| !startable.contains(id));
            let waiting = self.jobs.len() + startable.len() + parked.count();
            if !halted {
                // A halted run's tasks wait for the login or the network,
                // which `halted` says; no account would start them.
                self.summary.waiting_for_account = u32::try_from(waiting).unwrap_or(u32::MAX);
            }
            if frozen || halted || self.mode == Mode::Once || !(hooks.idle)() {
                return Ok(());
            }
            self.sweep_review(false)?;
        }
    }

    /// Ready tasks of this repository whose dependencies are all done, in
    /// board order (priority, then age).
    fn startable(&self) -> PortResult<Vec<String>> {
        let store = &self.deps.store;
        let repo = Some(self.deps.settings.repo_root.as_str());
        let mut ids = Vec::new();
        for task in store.list_tasks(repo, Some(TaskState::Ready))? {
            if self.mode == Mode::Once && self.started.contains(&task.id) {
                continue;
            }
            let mut open = false;
            for dep in store.deps_of(&task.id)? {
                // A dependency that no longer exists can never be done.
                open |= !store
                    .get_task(&dep)
                    .is_ok_and(|t| t.state == TaskState::Done);
            }
            if !open {
                ids.push(task.id);
            }
        }
        Ok(ids)
    }

    fn start_jobs<'scope>(&mut self, scope: &'scope Scope<'scope, '_>) -> PortResult<()>
    where
        'a: 'scope,
    {
        let limit = self.deps.settings.max_parallel.max(1) as usize;
        while self.active < limit {
            let job = match self.jobs.pop_front() {
                Some(job) => job,
                None => match self.startable()?.into_iter().next() {
                    Some(task_id) => Job::Agent { task_id },
                    None => break,
                },
            };
            let purpose = match job {
                Job::Audit { .. } => Purpose::Audit,
                _ => Purpose::Work,
            };
            let Some(grant) = self.deps.accounts.acquire_for(job.task_id(), purpose)? else {
                // No account this job may run on is free (all busy or
                // cooling down, or the one it is bound to): wait, never
                // fail. A new task is simply found again on the next pass.
                if !matches!(job, Job::Agent { .. }) {
                    self.jobs.push_front(job);
                }
                break;
            };
            if let Job::Agent { task_id } = &job {
                self.deps
                    .move_to(task_id, TaskState::Running, None, "picked by aaf run")?;
                self.started.insert(task_id.clone());
                self.parked.remove(task_id);
                // Running again after a usage limit: no longer just requeued.
                self.summary.requeued.retain(|id| id != task_id);
                self.summary.agent_runs += 1;
            }
            self.active += 1;
            let deps = self.deps;
            let tx = self.msg_tx.clone();
            scope.spawn(move || {
                let task_id = job.task_id().to_string();
                let outcome = catch_unwind(AssertUnwindSafe(|| worker::run_job(deps, job, &grant)))
                    .unwrap_or_else(|_| worker::block_infra(deps, &task_id, "the worker panicked"));
                // Released before the scheduler hears about it, so the
                // account is free for the job it starts next.
                drop(grant);
                // The receiver is gone only when the scheduler already
                // returned an error; nothing is left to tell.
                let _ = tx.send(Msg::Worker(outcome));
            });
        }
        Ok(())
    }

    fn queue_gate(&mut self, job: GateJob) -> PortResult<()> {
        self.in_gate += 1;
        self.gate_tx
            .send(job)
            .map_err(|_| PortError::Io("the gate thread is gone".to_string()))
    }

    fn handle(&mut self, msg: Msg) -> PortResult<()> {
        match msg {
            Msg::Worker(outcome) => {
                self.active -= 1;
                self.summary.errors.extend(outcome.error);
                match outcome.next {
                    // The worker is already gone: the task waits for the
                    // gate without holding a slot or an account.
                    Next::Gate(job) => self.queue_gate(job)?,
                    Next::Ready => self.summary.requeued.push(outcome.task_id),
                    Next::Switch => {
                        // The limit was the account's, and that account is
                        // cooling down now: in `Mode::Once` the task may
                        // start again on another one instead of waiting for
                        // the next `aaf run`. One restart per known account
                        // at most, so an account that never cools down (a
                        // reset time in the past) cannot keep the run going.
                        let limit = self.deps.accounts.profiles()?.len();
                        let restarts = self.restarts.entry(outcome.task_id.clone()).or_insert(0);
                        if *restarts < limit {
                            *restarts += 1;
                            self.started.remove(&outcome.task_id);
                        }
                        self.summary.requeued.push(outcome.task_id);
                    }
                    Next::Wait => {
                        // It stays in `started`: this invocation does not
                        // run it again, on this account or on another.
                        self.parked.insert(outcome.task_id.clone());
                        self.summary.requeued.push(outcome.task_id);
                    }
                    Next::Halt(why) => {
                        // An agent run's task is back in Ready; an audit's
                        // stays in Gating for the next run's gate.
                        let state = self.deps.store.get_task(&outcome.task_id)?.state;
                        if state == TaskState::Ready {
                            self.summary.requeued.push(outcome.task_id);
                        }
                        // The first one says it; a second worker failing the
                        // same way in parallel adds nothing new.
                        self.summary.halted.get_or_insert(why);
                    }
                    Next::Blocked => self.summary.blocked.push(outcome.task_id),
                }
            }
            Msg::Gate { job, resolution } => {
                self.in_gate -= 1;
                match resolution {
                    Ok(Resolution::Settled(state)) => match state {
                        TaskState::Done => self.summary.done.push(job.task_id),
                        TaskState::Review => self.summary.review.push(job.task_id),
                        _ => self.summary.blocked.push(job.task_id),
                    },
                    Ok(Resolution::NeedsAudit { lock_fp }) => {
                        self.jobs.push_back(Job::Audit { job, lock_fp });
                    }
                    Ok(Resolution::Failed(signal)) => {
                        self.jobs.push_back(Job::Unblock { job, signal });
                    }
                    Ok(Resolution::Retry) => self.queue_gate(job)?,
                    Err(error) => {
                        let outcome = worker::block_infra(self.deps, &job.task_id, &error);
                        self.summary.errors.extend(outcome.error);
                        self.summary.blocked.push(job.task_id);
                    }
                }
            }
        }
        Ok(())
    }

    /// A task in Review whose branch has reached the base is done: this is
    /// how a merged pull request releases the tasks that depend on it.
    fn sweep_review(&mut self, base_is_fresh: bool) -> PortResult<()> {
        let deps = self.deps;
        let repo = Some(deps.settings.repo_root.as_str());
        let review = deps.store.list_tasks(repo, Some(TaskState::Review))?;
        // Offline: the tasks stay in Review and are looked at again on the
        // next sweep.
        if review.is_empty() || !(base_is_fresh || self.fetch_base()) {
            return Ok(());
        }
        for task in review {
            if deps.git.is_merged(&task.id)? {
                deps.move_to(
                    &task.id,
                    TaskState::Done,
                    None,
                    "branch is merged into the base",
                )?;
                finish::close(deps, &task)?;
                if let Err(err) = deps.git.cleanup(&task.id) {
                    // The task is done either way; the leftover worktree
                    // only costs disk.
                    self.summary.errors.push(format!("{}: {err}", task.id));
                }
                self.summary.done.push(task.id);
            }
        }
        Ok(())
    }

    /// False when the remote could not be reached; the run goes on with the
    /// base it has.
    fn fetch_base(&mut self) -> bool {
        match self.deps.git.fetch_base() {
            Ok(()) => true,
            Err(err) => {
                self.summary.errors.push(format!("fetch: {err}"));
                false
            }
        }
    }
}

fn gate_loop(deps: &Deps, queue: &Receiver<GateJob>, tx: &Sender<Msg>, stats: &GateStats) {
    while let Ok(first) = queue.recv() {
        // Everything that queued up while the previous round ran becomes
        // one batch: the longer the queue, the larger the batch.
        let mut jobs = vec![first];
        jobs.extend(queue.try_iter());
        let pending = Mutex::new(jobs.clone());
        let round = catch_unwind(AssertUnwindSafe(|| {
            gate_round(deps, jobs, tx, stats, &pending);
        }));
        if round.is_err() {
            // Every task of the round still has to be answered, or the
            // scheduler would wait for it forever.
            let left = pending.lock().map(|mut p| std::mem::take(&mut *p));
            for job in left.unwrap_or_default() {
                report(tx, job, Err("the gate thread panicked".to_string()));
            }
        }
    }
}

fn report(tx: &Sender<Msg>, job: GateJob, resolution: Result<Resolution, String>) {
    // The receiver is gone only when the scheduler already returned an
    // error; nothing is left to tell.
    let _ = tx.send(Msg::Gate { job, resolution });
}

fn gate_round(
    deps: &Deps,
    jobs: Vec<GateJob>,
    tx: &Sender<Msg>,
    stats: &GateStats,
    pending: &Mutex<Vec<GateJob>>,
) {
    judge_round(deps, jobs, stats, &|job, resolution| {
        if let Ok(mut pending) = pending.lock() {
            pending.retain(|p| p.task_id != job.task_id);
        }
        report(tx, job, resolution);
    });
}

/// One gate round: judges `jobs` with as few gate runs as possible and hands
/// every job's resolution to `answer`, in board order.
pub(super) fn judge_round(
    deps: &Deps,
    jobs: Vec<GateJob>,
    stats: &GateStats,
    answer: &dyn Fn(GateJob, Result<Resolution, String>),
) {
    let member = |job: &GateJob| Member {
        task_id: job.task_id.clone(),
        worktree: job.worktree.clone(),
    };

    let mut batch: Vec<GateJob> = Vec::new();
    let mut alone: Vec<GateJob> = Vec::new();
    for job in jobs {
        match deps.gate.head_lock(&job.worktree.path) {
            Ok(lock) if lock.status == LockStatus::Clear => batch.push(job),
            Ok(lock)
                if lock.status == LockStatus::Blocked
                    && !job.audited
                    && !lock.lock_fp.is_empty() =>
            {
                let lock_fp = lock.lock_fp;
                answer(job, Ok(Resolution::NeedsAudit { lock_fp }));
            }
            // Approved, still blocked after its audit, or unreadable: the
            // approval lives with the task's own worktree, so that is where
            // the gate has to run, and its receipt says what is wrong.
            _ => alone.push(job),
        }
    }

    let members: Vec<Member> = batch.iter().map(member).collect();
    let mut outcome = gate_batch(&*deps.git, &*deps.gate, &members);
    for job in &alone {
        let single = gate_batch(&*deps.git, &*deps.gate, &[member(job)]);
        outcome.verdicts.extend(single.verdicts);
        outcome.gate_runs += single.gate_runs;
    }
    stats.runs.fetch_add(outcome.gate_runs, Ordering::Relaxed);
    if let Ok(mut warnings) = stats.warnings.lock() {
        warnings.append(&mut outcome.warnings);
    }

    // Only the batch has groups: a task gated alone has no integration tree.
    let mut round = Round {
        groups: std::mem::take(&mut outcome.groups),
        gate_runs: &stats.runs,
        warnings: &stats.warnings,
    };
    // Board order, so the merges of a clear batch land by priority.
    for job in batch.into_iter().chain(alone) {
        let Some(at) = outcome
            .verdicts
            .iter()
            .position(|j| j.task_id == job.task_id)
        else {
            answer(job, Err("the gate returned no verdict".to_string()));
            continue;
        };
        let judged = outcome.verdicts.swap_remove(at);
        let resolution = finish::settle(deps, &job, judged, &mut round).map_err(|e| e.to_string());
        answer(job, resolution);
    }
}
