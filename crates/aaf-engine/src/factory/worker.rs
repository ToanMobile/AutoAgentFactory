//! Jobs that need an account: the agent run of a task, the cross-audit of
//! its locked changes, and the unblocker ladder after a gate failure. Each
//! runs on its own thread and ends by saying where the task goes next.

use std::path::Path;
use std::sync::{Mutex, PoisonError};

use aaf_domain::budget::{BudgetCheck, RootBudget};
use aaf_domain::event::EventKind;
use aaf_domain::ports::{
    AgentOutcome, AgentRequest, AgentRunner, InfraFailure, InfraKind, PortError, PortResult,
};
use aaf_domain::run::{RunFinish, RunRecord, RunStatus};
use aaf_domain::task::{Task, TaskState};
use serde_json::json;

use super::ports::Grant;
use super::prompt::{build_prompt, sandbox_policy};
use super::{Deps, unblock};
use crate::cross_audit::{self, AUDIT_EXECUTOR, AuditInput, AuditReport, AuditVerdict};
use crate::git::TaskWorktree;
use crate::hook_settings;
use crate::unblocker::FailureSignal;

const NO_CHANGE: &str = "the agent run ended without changing anything on the branch";

/// A task waiting for the gate.
#[derive(Debug, Clone)]
pub(crate) struct GateJob {
    pub task_id: String,
    pub worktree: TaskWorktree,
    /// The cross-audit already ran for the current locked changes.
    pub audited: bool,
}

pub(crate) enum Job {
    Agent {
        task_id: String,
    },
    Audit {
        job: GateJob,
        lock_fp: String,
    },
    Unblock {
        job: GateJob,
        signal: Box<FailureSignal>,
    },
}

impl Job {
    pub(crate) fn task_id(&self) -> &str {
        match self {
            Job::Agent { task_id } => task_id,
            Job::Audit { job, .. } | Job::Unblock { job, .. } => &job.task_id,
        }
    }
}

/// Where the task is after a job.
pub(crate) enum Next {
    Gate(GateJob),
    Ready,
    /// Ready again because the account ran into its usage limit: the task
    /// itself has not been tried yet, and another account may take it.
    Switch,
    /// Ready again after a usage limit that no other account may take over
    /// (a pinned repository, or every account limited): the task waits for
    /// a cooldown and is not started again in this invocation.
    Wait,
    /// Ready again, untried: the agent CLI failed for its account or the
    /// network. Every task started after it would fail the same way, so
    /// the run stops; the text says why and what to fix.
    Halt(String),
    Blocked,
}

pub(crate) struct Outcome {
    pub task_id: String,
    pub next: Next,
    pub error: Option<String>,
}

pub(crate) fn run_job(deps: &Deps, job: Job, grant: &Grant) -> Outcome {
    let task_id = job.task_id().to_string();
    let result = match job {
        Job::Agent { task_id } => agent(deps, &task_id, grant),
        Job::Audit { job, lock_fp } => audit(deps, job, &lock_fp, grant),
        Job::Unblock { job, signal } => {
            unblock::handle(deps, &job.task_id, Some(&job.worktree), *signal, grant)
        }
    };
    match result {
        Ok(next) => Outcome {
            task_id,
            next,
            error: None,
        },
        Err(err) => block_infra(deps, &task_id, &err.to_string()),
    }
}

/// An error of the factory's own machinery (git, store, executor) is not
/// something an agent retry fixes: the task waits for a human.
pub(crate) fn block_infra(deps: &Deps, task_id: &str, error: &str) -> Outcome {
    let why = format!("factory error: {error}");
    let mut message = format!("{task_id}: {error}");
    if let Err(second) = unblock::block_task(deps, task_id, &why) {
        // The store itself is failing; the summary is the only place left.
        message.push_str(&format!(" (and the task could not be blocked: {second})"));
    }
    Outcome {
        task_id: task_id.to_string(),
        next: Next::Blocked,
        error: Some(message),
    }
}

pub(crate) fn ensure_budget(deps: &Deps, root_id: &str) -> PortResult<RootBudget> {
    match deps.store.get_budget(root_id) {
        Err(PortError::NotFound(_)) => {
            let budget = RootBudget::new(root_id, &deps.settings.repo.unblocker);
            deps.store.put_budget(&budget)?;
            Ok(budget)
        }
        other => other,
    }
}

/// The request of a run on `grant`'s account in `cwd`; the prompt is filled
/// in by the caller.
pub(crate) fn request(deps: &Deps, cwd: &str, grant: &Grant) -> PortResult<AgentRequest> {
    let settings = &deps.settings;
    // Every agent the factory starts (task run, cross-audit, subtask
    // proposal) gets its request here: none starts where the hook is not
    // registered.
    if let Some(problem) = hook_problem(deps, Path::new(cwd)) {
        return Err(PortError::Invalid(problem));
    }
    Ok(AgentRequest {
        run_id: unblock::unique_id(),
        prompt: String::new(),
        cwd: cwd.to_string(),
        account: grant.account.clone(),
        timeout_secs: settings.repo.run.run_timeout_secs,
        sandbox: sandbox_policy(
            settings.sandboxed,
            &settings.data_dir,
            &settings.home,
            grant.account.as_ref(),
            &deps.accounts.profiles()?,
        ),
    })
}

/// Why an agent started in `cwd` would run without `aaf-hook`; `None` when
/// it is registered there or the executor has no such hook.
pub(crate) fn hook_problem(deps: &Deps, cwd: &Path) -> Option<String> {
    let settings = &deps.settings;
    if !settings.hook_required {
        return None;
    }
    // The main checkout is the owner's and no agent writes there; every
    // other directory is a worktree cut from the base, and its settings
    // file must still be the base's.
    let base = if cwd == Path::new(&settings.repo_root) {
        None
    } else {
        match deps.git.base_sha() {
            Ok(base) => Some(base),
            Err(err) => return Some(format!("the base branch could not be read ({err})")),
        }
    };
    hook_settings::check_worktree(cwd, &settings.agent_path, base.as_deref()).err()
}

struct OpenRun {
    request: AgentRequest,
    started: i64,
}

fn begin_run(
    deps: &Deps,
    task: &Task,
    worktree: &TaskWorktree,
    executor: &str,
    grant: &Grant,
) -> PortResult<OpenRun> {
    let request = request(deps, &worktree.path.to_string_lossy(), grant)?;
    let started = deps.now();
    deps.store.insert_run(&RunRecord {
        id: request.run_id.clone(),
        task_id: task.id.clone(),
        account_id: grant.account_id().map(str::to_string),
        executor: executor.to_string(),
        worktree_path: request.cwd.clone(),
        branch: worktree.branch.clone(),
        status: RunStatus::Running,
        exit_code: None,
        tokens_in: 0,
        tokens_out: 0,
        started_at: started,
        ended_at: None,
    })?;
    deps.emit(
        EventKind::RunStarted,
        &task.id,
        json!({ "run_id": request.run_id, "executor": executor, "account": grant.account_id() }),
    )?;
    Ok(OpenRun { request, started })
}

/// Closes the run and charges the root budget: tokens are input + output +
/// cache creation (`tokens_in` already includes cache creation).
fn end_run(
    deps: &Deps,
    task: &Task,
    run: &OpenRun,
    status: RunStatus,
    exit_code: Option<i32>,
    tokens: (u64, u64),
) -> PortResult<()> {
    let ended = deps.now();
    deps.store.finish_run(
        &run.request.run_id,
        &RunFinish {
            status,
            exit_code,
            tokens_in: tokens.0,
            tokens_out: tokens.1,
            ended_at: ended,
        },
    )?;
    let wall = u64::try_from(ended - run.started).unwrap_or(0);
    ensure_budget(deps, &task.root_id)?;
    deps.store
        .add_usage(&task.root_id, tokens.0.saturating_add(tokens.1), wall, 0)?;
    deps.emit(
        EventKind::RunFinished,
        &task.id,
        json!({
            "run_id": run.request.run_id,
            "status": status.as_str(),
            "tokens_in": tokens.0,
            "tokens_out": tokens.1,
        }),
    )
}

fn status_of(outcome: &AgentOutcome) -> RunStatus {
    if outcome.limit_hit.is_some() {
        RunStatus::LimitHit
    } else if outcome.timed_out {
        RunStatus::TimedOut
    } else if outcome.exit_code == Some(0) {
        RunStatus::Succeeded
    } else {
        RunStatus::Failed
    }
}

fn agent(deps: &Deps, task_id: &str, grant: &Grant) -> PortResult<Next> {
    let task = deps.store.get_task(task_id)?;
    let budget = ensure_budget(deps, &task.root_id)?.check();
    if budget != BudgetCheck::Ok {
        // No run on an exhausted budget; the ladder freezes the root.
        let mut signal = FailureSignal::empty();
        signal.budget = budget;
        return unblock::handle(deps, task_id, None, signal, grant);
    }
    let worktree = deps.git.task_worktree(task_id)?;
    if !deps.store.runs_for_task(task_id)?.is_empty() {
        // A failed or interrupted run leaves its edits uncommitted, and git
        // refuses to rebase a dirty tree: keep them as a commit, the next
        // attempt builds on them.
        deps.git
            .commit_all(&worktree, &format!("aaf: {} (earlier attempt)", task.title))?;
        // A rerun picks up what landed on the base since (finished
        // subtasks, other tasks). A conflict is aborted and left for the
        // merge step, where it is classified as one.
        deps.git.rebase(&worktree)?;
    } else if hook_problem(deps, &worktree.path).is_some() {
        // The worktree may have been cut before the registration was
        // committed on the base: nothing ran in it yet, so it can follow.
        deps.git.rebase(&worktree)?;
    }
    if let Some(problem) = hook_problem(deps, &worktree.path) {
        // Not a failure a retry fixes, and no run is recorded: nothing ran.
        // Nor a failed root: the task was never tried.
        unblock::block_untried(deps, task_id, &problem)?;
        return Ok(Next::Blocked);
    }

    let mut run = begin_run(
        deps,
        &task,
        &worktree,
        &deps.settings.repo.run.executor,
        grant,
    )?;
    run.request.prompt = build_prompt(&task, &worktree.branch);
    let mut outcome = match deps.runner.run(&run.request) {
        Ok(outcome) => outcome,
        Err(err) => {
            end_run(deps, &task, &run, RunStatus::Failed, None, (0, 0))?;
            return Err(err);
        }
    };
    let tokens = (outcome.tokens_in, outcome.tokens_out);
    end_run(
        deps,
        &task,
        &run,
        status_of(&outcome),
        outcome.exit_code,
        tokens,
    )?;
    if let (Some(hit), Some(account)) = (&outcome.limit_hit, grant.account_id()) {
        deps.accounts.report_limit(account, hit)?;
        deps.emit(
            EventKind::AccountCooldown,
            task_id,
            // The provider's own words: the only record of why an account
            // was benched once the run's output is gone.
            json!({
                "account": account,
                "reset_at": hit.reset_at,
                "raw": unblock::one_line(&hit.raw, 300),
            }),
        )?;
    }
    if let Some(infra) = &outcome.infra {
        return halt_on_infra(deps, task_id, grant, infra);
    }

    if outcome.succeeded() {
        deps.git
            .commit_all(&worktree, &format!("aaf: {}", task.title))?;
        if let Some(problem) = hook_problem(deps, &worktree.path) {
            // The run changed its own guard. Nothing of it goes on to the
            // gate, a pull request or the base, where the changed file
            // would be loaded by the owner's own sessions.
            unblock::block_untried(deps, task_id, &problem)?;
            return Ok(Next::Blocked);
        }
        if deps.git.has_changes(&worktree)? {
            deps.move_to(task_id, TaskState::Gating, None, "agent run finished")?;
            return Ok(Next::Gate(GateJob {
                task_id: task_id.to_string(),
                worktree,
                audited: false,
            }));
        }
        // A fixed text, so repeated empty runs share one failure digest.
        outcome.log_tail = NO_CHANGE.to_string();
        deps.store
            .append_context_note(task_id, NO_CHANGE, deps.now())?;
    }
    let mut signal = FailureSignal::empty();
    signal.outcome = Some(outcome);
    unblock::handle(deps, task_id, Some(&worktree), signal, grant)
}

/// The agent CLI could not work for its account or network: the task goes
/// back to Ready without a ladder step (no note, no cycle, no failed root),
/// and the run is told to stop.
fn halt_on_infra(
    deps: &Deps,
    task_id: &str,
    grant: &Grant,
    infra: &InfraFailure,
) -> PortResult<Next> {
    let (what, fix) = infra_words(grant, infra);
    let raw = unblock::one_line(&infra.raw, 300);
    deps.move_to(
        task_id,
        TaskState::Ready,
        None,
        &format!("agent CLI failed: {what} ({raw}); not counted as an attempt"),
    )?;
    Ok(Next::Halt(halt_message(task_id, what, infra, &fix)))
}

/// What went wrong, and what a person does about it.
fn infra_words(grant: &Grant, infra: &InfraFailure) -> (&'static str, String) {
    match infra.kind {
        InfraKind::Auth => (
            "its login or credentials were refused",
            format!(
                "log in again with account {} (or fix its credentials)",
                grant.account_id().unwrap_or("default")
            ),
        ),
        InfraKind::Network => (
            "the provider could not be reached (network)",
            "fix the network, proxy or DNS".to_string(),
        ),
    }
}

fn halt_message(task_id: &str, what: &str, infra: &InfraFailure, fix: &str) -> String {
    format!(
        "{task_id}: the agent CLI failed: {what}: {}. The run stopped so no \
         other task fails for the same reason; {fix}, then run aaf run again",
        unblock::one_line(&infra.raw, 300)
    )
}

fn verdict_text(verdict: &AuditVerdict) -> String {
    match verdict {
        AuditVerdict::Pass { .. } => "pass".to_string(),
        AuditVerdict::Inconclusive { reason } => format!("inconclusive: {reason}"),
        AuditVerdict::Fail { findings, .. } => {
            let items: Vec<String> = findings
                .iter()
                .map(|f| format!("{} ({})", f.id, f.evidence))
                .collect();
            format!("fail: {}", items.join("; "))
        }
    }
}

/// Passes a run on to the real runner and keeps what an infra failure said:
/// the audit's report only knows that its run did not complete.
struct InfraWatch<'a> {
    runner: &'a dyn AgentRunner,
    seen: Mutex<Option<InfraFailure>>,
}

impl AgentRunner for InfraWatch<'_> {
    fn run(&self, request: &AgentRequest) -> PortResult<AgentOutcome> {
        let outcome = self.runner.run(request)?;
        if let Some(infra) = &outcome.infra {
            *self.seen.lock().unwrap_or_else(PoisonError::into_inner) = Some(infra.clone());
        }
        Ok(outcome)
    }
}

/// One recorded auditor run in `tree`, and the infra failure that ended it,
/// if one did.
fn audit_run(
    deps: &Deps,
    task: &Task,
    input: &AuditInput,
    tree: &TaskWorktree,
    grant: &Grant,
) -> PortResult<(AuditReport, Option<InfraFailure>)> {
    let run = begin_run(deps, task, tree, AUDIT_EXECUTOR, grant)?;
    let watch = InfraWatch {
        runner: &*deps.runner,
        seen: Mutex::new(None),
    };
    let report = match cross_audit::run_audit(input, &watch, &run.request) {
        Ok(report) => report,
        Err(err) => {
            end_run(deps, task, &run, RunStatus::Failed, None, (0, 0))?;
            return Err(err);
        }
    };
    let status = if report.verdict.is_pass() {
        RunStatus::Succeeded
    } else {
        RunStatus::Failed
    };
    let tokens = (report.tokens_in, report.tokens_out);
    end_run(deps, task, &run, status, None, tokens)?;
    let infra = watch
        .seen
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner);
    Ok((report, infra))
}

/// Cross-audit of a task whose locked files changed. A pass signs the
/// approval for the task's own worktree (approvals are per worktree, and
/// that is where its gate runs) and sends it back to the gate.
fn audit(deps: &Deps, job: GateJob, lock_fp: &str, grant: &Grant) -> PortResult<Next> {
    let task = deps.store.get_task(&job.task_id)?;
    let input = AuditInput {
        spec: task.spec.clone(),
        diff: deps.git.diff(&job.worktree)?,
        mobile: deps.settings.mobile,
    };
    let author = deps
        .store
        .runs_for_task(&task.id)?
        .into_iter()
        .rev()
        .find(|run| run.executor != AUDIT_EXECUTOR)
        .and_then(|run| run.account_id);
    // Unknown on either side counts as the same account: never overclaim
    // independence.
    let same_account = match (author.as_deref(), grant.account_id()) {
        (Some(author), Some(auditor)) => author == auditor,
        _ => true,
    };

    // The auditor gets the changes as text in its prompt and works in a
    // clean tree of the base. Started in the author's worktree it would load
    // the author's `CLAUDE.md` and `.claude/` settings (instructions, hooks,
    // env): the very files it may be auditing. The hook check of `request`
    // applies to that tree like to any other.
    let clean = deps.git.integrate(&[])?;
    let audited = audit_run(deps, &task, &input, &clean.tree, grant);
    // The verdict stands whether or not the tree could be removed: what is
    // left is an empty checkout of the base under its own unique name, which
    // costs disk and nothing else.
    let _ = deps.git.discard(&clean);
    let (report, infra) = audited?;
    if let Some(infra) = infra {
        // No verdict was reached, so none is recorded against the task. It
        // stays in Gating: the next run's gate asks for the audit again.
        let (what, fix) = infra_words(grant, &infra);
        return Ok(Next::Halt(halt_message(&task.id, what, &infra, &fix)));
    }

    let reviewer = format!("cross-audit:{}", grant.account_id().unwrap_or("default"));
    let approval = cross_audit::approve_if_pass(
        &report,
        &deps.settings.data_dir,
        &job.worktree.path,
        lock_fp,
        &reviewer,
        same_account,
        &*deps.clock,
    )?;
    let text = verdict_text(&report.verdict);
    deps.emit(
        EventKind::CrossAudit,
        &task.id,
        json!({ "verdict": text, "same_account": same_account, "lock_fp": lock_fp }),
    )?;
    if approval.is_some() {
        return Ok(Next::Gate(GateJob {
            audited: true,
            ..job
        }));
    }
    let note = format!("cross-audit rejected the changes to locked files ({text})");
    deps.store
        .append_context_note(&task.id, &unblock::one_line(&note, 400), deps.now())?;
    let mut signal = FailureSignal::empty();
    signal.outcome = Some(AgentOutcome {
        exit_code: Some(1),
        timed_out: false,
        final_text: String::new(),
        tokens_in: 0,
        tokens_out: 0,
        limit_hit: None,
        infra: None,
        // The verdict kind only: finding texts differ from run to run and
        // would give every rejection a new failure digest.
        log_tail: "cross-audit rejected the changes to locked files".to_string(),
    });
    unblock::handle(deps, &task.id, Some(&job.worktree), signal, grant)
}
