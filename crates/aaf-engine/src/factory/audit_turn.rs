//! Auditor turn execution (with 1-shot format-repair retry on `Inconclusive`),
//! short-circuit guard for empty/truncated diffs, and runner outcome watcher.

use std::sync::{Mutex, PoisonError};

use aaf_domain::budget::BudgetCheck;
use aaf_domain::event::EventKind;
use aaf_domain::ports::{
    AgentOutcome, AgentRequest, AgentRunner, InfraFailure, InfraKind, LimitHit, PortError,
    PortResult,
};
use aaf_domain::run::RunStatus;
use aaf_domain::task::{Task, TaskState};

use super::audit_backend::{AuditBackend, audit_cmd_is_claude};
use super::git_real::COMMIT_DEFERRED;
use super::ports::{AccountPool, Grant};
use super::scheduler::DEFAULT_RATE_LIMIT_COOLDOWN_SECS;
use super::worker::{self, Next};
use super::{Deps, unblock};
use crate::cross_audit::{self, AUDIT_EXECUTOR, AuditInput, AuditReport, AuditVerdict};
use crate::git::TaskWorktree;
use crate::unblocker::{FailureSignal, QuotaRoute};

/// An account whose login was refused rests this long while others work.
const AUTH_COOLDOWN_SECS: i64 = 3600;
/// A commit that may pass later (signing, a slow hook, the commit lock)
/// waits this long between tries ...
const COMMIT_WAIT_SECS: i64 = 900;
/// ... this many times for the same failure, then a person is asked.
const MAX_COMMIT_WAITS: usize = 2;
const COMMIT_WAIT_MARKER: &str = "commit_wait digest=";
const NOTE_TEXT_CHARS: usize = 1500;
const NO_CHANGE_REASON_CHARS: usize = 300;
const HISTORY_EVENTS: u32 = 50;

/// Passes a run on to the real runner and captures any infra failure, usage
/// limit, failed log tail, and final text: wrappers like audit and subtask
/// proposer only report their own result.
pub(super) struct InfraWatch<'a> {
    runner: &'a dyn AgentRunner,
    seen: Mutex<Option<InfraFailure>>,
    limit_hit: Mutex<Option<LimitHit>>,
    failed_tail: Mutex<Option<String>>,
    last_succeeded: Mutex<bool>,
    last_final_text: Mutex<String>,
}

impl<'a> InfraWatch<'a> {
    pub(super) fn new(runner: &'a dyn AgentRunner) -> Self {
        Self {
            runner,
            seen: Mutex::new(None),
            limit_hit: Mutex::new(None),
            failed_tail: Mutex::new(None),
            last_succeeded: Mutex::new(false),
            last_final_text: Mutex::new(String::new()),
        }
    }

    pub(super) fn take(self) -> (Option<InfraFailure>, Option<LimitHit>) {
        let (infra, hit, _, _, _) = self.take_all();
        (infra, hit)
    }

    pub(super) fn take_all(
        self,
    ) -> (
        Option<InfraFailure>,
        Option<LimitHit>,
        Option<String>,
        bool,
        String,
    ) {
        (
            self.seen
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner),
            self.limit_hit
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner),
            self.failed_tail
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner),
            self.last_succeeded
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner),
            self.last_final_text
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }
}

impl AgentRunner for InfraWatch<'_> {
    fn run(&self, request: &AgentRequest) -> PortResult<AgentOutcome> {
        let outcome = self.runner.run(request)?;
        *self
            .last_succeeded
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = outcome.succeeded();
        *self
            .last_final_text
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = outcome.final_text.clone();
        if let Some(infra) = &outcome.infra {
            *self.seen.lock().unwrap_or_else(PoisonError::into_inner) = Some(infra.clone());
        }
        if let Some(hit) = &outcome.limit_hit {
            *self
                .limit_hit
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(hit.clone());
        }
        if !outcome.succeeded()
            && outcome.infra.is_none()
            && outcome.limit_hit.is_none()
            && !outcome.log_tail.trim().is_empty()
        {
            *self
                .failed_tail
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(outcome.log_tail.clone());
        }
        Ok(outcome)
    }

    fn reports_tokens(&self) -> bool {
        self.runner.reports_tokens()
    }
}

/// Returns a concise diagnostic reason when `diff` cannot possibly be approved
/// by an auditor (empty or exceeding [`cross_audit::prompt::MAX_DIFF_BYTES`]),
/// allowing callers to short-circuit with 0 worktrees and 0 LLM tokens.
pub(super) fn short_circuit_audit_reason(diff: &str) -> Option<String> {
    if diff.trim().is_empty() {
        return Some("inconclusive: empty diff: nothing to audit".to_string());
    }
    if cross_audit::prompt::truncate_diff(diff).1 {
        return Some(format!(
            "inconclusive: diff was truncated ({} > {} bytes): the unseen part cannot be approved",
            diff.len(),
            cross_audit::prompt::MAX_DIFF_BYTES,
        ));
    }
    None
}

fn has_status_code(lower: &str, code: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "http ",
        "http/1.1 ",
        "http/2 ",
        "status ",
        "status: ",
        "status=",
        "code ",
        "code: ",
        "code=",
        "error ",
        "error: ",
    ];
    PREFIXES
        .iter()
        .any(|p| lower.contains(&format!("{p}{code}")))
}

/// Reads an auditor's failed log tail for a rate limit, a refused login or a
/// network failure. A rate limit found this way carries no reset time of its
/// own, so it gets the default pause from `now` (the account router would
/// otherwise cool the account down for its 5-hour unknown-reset fallback).
fn classify_auditor_log_tail(
    tail: &str,
    now: i64,
    infra: &mut Option<InfraFailure>,
    limit_hit: &mut Option<LimitHit>,
) {
    let lower = tail.to_ascii_lowercase();
    let raw = unblock::one_line(tail, 300);
    let has_any = |needles: &[&str]| needles.iter().any(|n| lower.contains(n));
    let quota_hit = has_status_code(&lower, "429")
        || has_any(&[
            "429 too many",
            "too many requests",
            "rate limit",
            "rate_limit",
            "quota",
        ]);
    let auth_hit = has_status_code(&lower, "401")
        || has_status_code(&lower, "403")
        || has_any(&[
            "401 unauthorized",
            "403 forbidden",
            "unauthorized",
            "invalid api key",
            "invalid_api_key",
            "authentication failed",
        ]);
    let net_hit = has_any(&[
        "econnrefused",
        "etimedout",
        "econnreset",
        "enotfound",
        "ehostunreach",
        "connection refused",
        "connection timed out",
        "503 service unavailable",
        "529 overloaded",
    ]) || ["502", "503", "504", "529"]
        .iter()
        .any(|c| has_status_code(&lower, c));
    if quota_hit {
        *limit_hit = Some(LimitHit {
            reset_at: Some(now.saturating_add(DEFAULT_RATE_LIMIT_COOLDOWN_SECS)),
            raw,
        });
    } else if auth_hit {
        *infra = Some(InfraFailure {
            kind: InfraKind::Auth,
            raw,
        });
    } else if net_hit {
        *infra = Some(InfraFailure {
            kind: InfraKind::Network,
            raw,
        });
    }
}

/// One recorded auditor run in `tree`, plus any infra failure or rate limit.
/// Performs at most one format-repair retry when the auditor succeeded with
/// unparseable / schema-invalid output (`Inconclusive`) and budget remains.
pub(crate) fn run_auditor_turn(
    deps: &Deps,
    audit_backend: Option<&AuditBackend>,
    task: &Task,
    input: &AuditInput,
    tree: &TaskWorktree,
    grant: &Grant,
    require_scores: bool,
) -> PortResult<(AuditReport, Option<InfraFailure>, Option<LimitHit>)> {
    let custom_cmd = deps.settings.repo.cross_audit.executor_cmd.trim();
    let hook_required = audit_backend.map_or(deps.settings.hook_required, |a| a.hook_required)
        || (!custom_cmd.is_empty() && audit_cmd_is_claude(custom_cmd));
    let accounts: &dyn AccountPool = audit_backend.map_or(&*deps.accounts, |a| &*a.accounts);
    let runner = audit_backend.map_or(&*deps.runner, |a| &*a.runner);
    let mut run = worker::begin_run_with(
        deps,
        task,
        tree,
        AUDIT_EXECUTOR,
        grant,
        hook_required,
        accounts,
    )?;
    run.request.sandbox.read_only = true;
    let watch = InfraWatch::new(runner);
    let tokens_measured = watch.reports_tokens();
    let mut report =
        match cross_audit::run_scored_audit(input, &watch, &run.request, require_scores) {
            Ok(report) => report,
            Err(err) => {
                worker::end_run(deps, task, &run, RunStatus::Failed, None, (0, 0), false)?;
                return Err(err);
            }
        };
    let (mut infra, mut limit_hit, failed_tail, succeeded, final_text) = watch.take_all();
    if limit_hit.is_none()
        && infra.is_none()
        && let Some(tail) = failed_tail.as_deref()
    {
        classify_auditor_log_tail(tail, deps.now(), &mut infra, &mut limit_hit);
    }

    if succeeded
        && !report.diff_truncated
        && infra.is_none()
        && limit_hit.is_none()
        && !final_text.trim().is_empty()
        && final_text.chars().count() <= cross_audit::prompt::MAX_REPAIR_PRIOR_CHARS
        && let AuditVerdict::Inconclusive { ref reason } = report.verdict
    {
        let mut projected = worker::ensure_budget(deps, &task.root_id)?;
        projected.add_usage(report.tokens_in.saturating_add(report.tokens_out), 0, 0);
        if projected.check() == BudgetCheck::Ok {
            let mut repair_req = run.request.clone();
            repair_req.run_id = unblock::unique_id();
            repair_req.prompt = cross_audit::prompt::build_format_repair_prompt(
                input,
                require_scores,
                reason,
                &final_text,
            );
            // Fall back to the original Inconclusive verdict if the runner errors
            // on the retry (e.g. transient port failure or single-shot test mock).
            if let Ok(repair_out) = runner.run(&repair_req) {
                report.tokens_in = report.tokens_in.saturating_add(repair_out.tokens_in);
                report.tokens_out = report.tokens_out.saturating_add(repair_out.tokens_out);
                if let Some(hit) = repair_out.limit_hit {
                    limit_hit = Some(hit);
                } else if let Some(inf) = repair_out.infra {
                    infra = Some(inf);
                } else if !repair_out.succeeded() {
                    if !repair_out.log_tail.trim().is_empty() {
                        let tail = &repair_out.log_tail;
                        classify_auditor_log_tail(tail, deps.now(), &mut infra, &mut limit_hit);
                    }
                } else {
                    let (visible_diff, _) = cross_audit::prompt::truncate_diff(&input.diff);
                    let hunks = cross_audit::parse_diff_hunks_with_truncation(visible_diff, false);
                    let parsed = cross_audit::validate_verdict_against_diff(
                        cross_audit::parse_scored_verdict(
                            &repair_out.final_text,
                            &report.checklist,
                            require_scores,
                        ),
                        &hunks,
                    );
                    // A blocker the first reply named (but left uncited or
                    // cited outside the diff) may be cited now, never dropped.
                    let named_blocker =
                        cross_audit::verdict::names_blocking_yes(&final_text, &report.checklist);
                    if !matches!(parsed.verdict, AuditVerdict::Inconclusive { .. })
                        && !(named_blocker && parsed.verdict.is_pass())
                    {
                        report.verdict = parsed.verdict;
                        report.scores = parsed.scores;
                    }
                }
            }
        }
    }

    let status = if limit_hit.is_some() {
        RunStatus::LimitHit
    } else if report.verdict.is_pass() {
        RunStatus::Succeeded
    } else {
        RunStatus::Failed
    };
    let tokens = (report.tokens_in, report.tokens_out);
    worker::end_run(deps, task, &run, status, None, tokens, tokens_measured)?;
    Ok((report, infra, limit_hit))
}

/// What went wrong, and what a person does about it.
pub(super) fn infra_words(grant: &Grant, infra: &InfraFailure) -> (&'static str, String) {
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

pub(super) fn halt_message(task_id: &str, what: &str, infra: &InfraFailure, fix: &str) -> String {
    format!(
        "{task_id}: the agent CLI failed: {what}: {}. The run stopped so no \
         other task fails for the same reason; {fix}, then run aaf run again",
        unblock::one_line(&infra.raw, 300)
    )
}

/// The agent CLI could not work for its account or network: the task goes
/// back to Ready without a ladder step (no note, no cycle, no failed root).
/// Network failures return [`Next::RetryAfter`]; an auth failure moves the task to
/// another account when one can take it, and halts otherwise.
pub(super) fn halt_on_infra(
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
    let detail = halt_message(task_id, what, infra, &fix);
    if infra.kind == InfraKind::Auth && switch_after_auth(deps, task_id, grant, infra)? {
        return Ok(Next::Switch);
    }
    if infra.kind == InfraKind::Network {
        return Ok(Next::RetryAfter {
            until: deps.now(),
            attempt: 1,
            detail,
        });
    }
    Ok(Next::Halt(detail))
}

/// O12-R10-2: a login refused on one account says nothing about the others:
/// the account rests and another one takes the task. False when no other
/// account can (a single or pinned account, all others resting), and the run
/// halts as before.
fn switch_after_auth(
    deps: &Deps,
    task_id: &str,
    grant: &Grant,
    infra: &InfraFailure,
) -> PortResult<bool> {
    if grant.account_id().is_none()
        || deps.accounts.provider_account_count()? < 2
        || deps.accounts.after_limit()? == Some(QuotaRoute::Wait)
    {
        return Ok(false);
    }
    let hit = LimitHit {
        reset_at: Some(deps.now().saturating_add(AUTH_COOLDOWN_SECS)),
        raw: format!("auth: {}", unblock::one_line(&infra.raw, 300)),
    };
    unblock::report_account_limit(deps, task_id, grant, &hit)?;
    // Asked after the report: the refused account already counts as resting.
    Ok(deps.accounts.after_limit()? != Some(QuotaRoute::Wait))
}

/// O12-R10-6: what the agent said about a run that changed nothing, for the
/// next run. The failure digest does not see it (it varies run to run).
pub(super) fn note_no_change_reason(
    deps: &Deps,
    task_id: &str,
    final_text: &str,
) -> PortResult<()> {
    let line = unblock::one_line(final_text, NO_CHANGE_REASON_CHARS);
    if line.is_empty() {
        return Ok(());
    }
    let safe = unblock::sanitize_untrusted_close_tag(&line);
    let note = format!("no_change_reason: <untrusted-data>{safe}</untrusted-data>");
    deps.store.append_context_note(task_id, &note, deps.now())
}

/// O12-R10-3 (c): the edits of an earlier run are kept as a commit before the
/// rebase. A commit the repository refuses is left for the agent to fix: it
/// works on the tree as it is, without the rebase.
pub(super) fn commit_earlier_attempt(
    deps: &Deps,
    task: &Task,
    worktree: &TaskWorktree,
) -> PortResult<Option<Next>> {
    let message = format!("aaf: {} (earlier attempt)", task.title);
    match deps.git.commit_all(worktree, &message) {
        // A rerun picks up what landed on the base since (finished
        // subtasks, other tasks). A conflict is aborted and left for the
        // merge step, where it is classified as one.
        Ok(()) => deps.git.rebase(worktree).map(|_| None),
        Err(PortError::Conflict(_)) => Ok(None),
        Err(PortError::Io(why)) if why.starts_with(COMMIT_DEFERRED) => {
            wait_for_commit(deps, &task.id, &why).map(Some)
        }
        Err(other) => Err(other),
    }
}

/// O12-R10-3 (b), (e): the commit of a finished run failed. A refusal the
/// agent can fix (a commit hook, a nested repository) goes back to it through
/// the unblock ladder with the refusal in its notes; a failure that may pass
/// later waits; anything else is the factory's own error.
pub(super) fn after_commit_failure(
    deps: &Deps,
    task_id: &str,
    worktree: &TaskWorktree,
    mut outcome: AgentOutcome,
    error: PortError,
    grant: &Grant,
) -> PortResult<Next> {
    match error {
        PortError::Conflict(why) => {
            let safe =
                unblock::sanitize_untrusted_close_tag(&unblock::one_line(&why, NOTE_TEXT_CHARS));
            let note = format!(
                "commit_refused: the commit of the last run was refused; fix what it \
                 says, keep the fix on this branch: \
                 <untrusted-data id=\"commit-refusal\">{safe}</untrusted-data>"
            );
            deps.store.append_context_note(task_id, &note, deps.now())?;
            outcome.log_tail = why;
            let mut signal = FailureSignal::empty();
            signal.outcome = Some(outcome);
            unblock::handle(deps, task_id, Some(worktree), signal, grant)
        }
        PortError::Io(why) if why.starts_with(COMMIT_DEFERRED) => {
            wait_for_commit(deps, task_id, &why)
        }
        other => Err(other),
    }
}

/// O12-R10-3 (e): a commit that may pass unchanged later waits, at most
/// [`MAX_COMMIT_WAITS`] times for the same failure since the task was last
/// blocked; then the error goes the factory-error way, to a person.
fn wait_for_commit(deps: &Deps, task_id: &str, why: &str) -> PortResult<Next> {
    let stable: String = why.chars().filter(|c| !c.is_ascii_digit()).collect();
    let digest: String = aaf_fp::sha256_hex(stable.as_bytes())
        .chars()
        .take(12)
        .collect();
    let marker = format!("{COMMIT_WAIT_MARKER}{digest}");
    let mut waited = 0;
    for event in deps.store.events_for_task(task_id, HISTORY_EVENTS)? {
        if event.kind != EventKind::TaskStateChanged {
            continue;
        }
        let payload: serde_json::Value =
            serde_json::from_str(&event.payload_json).unwrap_or_default();
        if payload["to"].as_str() == Some(TaskState::Blocked.as_str()) {
            break;
        }
        if payload["why"]
            .as_str()
            .is_some_and(|w| w.starts_with(&marker))
        {
            waited += 1;
        }
    }
    if waited >= MAX_COMMIT_WAITS {
        return Err(PortError::Io(why.to_string()));
    }
    let until = deps.now().saturating_add(COMMIT_WAIT_SECS);
    let note = format!("{marker}: {}", unblock::one_line(why, 300));
    deps.move_to(task_id, TaskState::Ready, None, &note)?;
    Ok(Next::Wait { until: Some(until) })
}
