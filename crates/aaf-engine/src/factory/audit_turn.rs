//! Auditor turn execution (with up to 2 format/citation-repair retries on
//! `Inconclusive`), lockfile-summarized short-circuit guard for empty/truncated
//! diffs, and runner outcome watcher.

use aaf_domain::budget::BudgetCheck;
use aaf_domain::event::EventKind;
use aaf_domain::ports::{
    AgentOutcome, AgentRunner, InfraFailure, InfraKind, LimitHit, PortError, PortResult,
};
use aaf_domain::run::RunStatus;
use aaf_domain::task::{Task, TaskState};
mod watch;

pub(super) use watch::InfraWatch;

use super::adapters::provider_for;
use super::audit_backend::{AuditBackend, audit_cmd_is_claude};
use super::git_real::COMMIT_DEFERRED;
use super::ports::{AccountPool, Grant};
use super::scheduler::DEFAULT_RATE_LIMIT_COOLDOWN_SECS;
use super::stage_router::StageTarget;
use super::worker::{self, Next};
use super::{Deps, unblock};
use crate::cross_audit::{self, AUDIT_EXECUTOR, AuditInput, AuditReport, AuditVerdict};
use crate::git::TaskWorktree;
use crate::unblocker::{FailureSignal, QuotaRoute};

/// An account whose login was refused rests this long while others work.
const AUTH_QUARANTINE_SECS: i64 = 3600;
/// A commit that may pass later (signing, a slow hook, the commit lock)
/// waits this long between tries ...
const COMMIT_WAIT_SECS: i64 = 900;
/// ... this many times for the same failure, then a person is asked.
const MAX_COMMIT_WAITS: usize = 2;
const COMMIT_WAIT_MARKER: &str = "commit_wait digest=";
const NOTE_TEXT_CHARS: usize = 1500;
const NO_CHANGE_REASON_CHARS: usize = 300;
const HISTORY_EVENTS: u32 = 50;

/// Maximum format/citation-repair retries when an auditor returns `Inconclusive`.
pub(super) const MAX_AUDITOR_INCONCLUSIVE_RETRIES: usize = 2;

/// Returns a concise diagnostic reason when `diff` (after deterministic
/// allowlisted lockfile summarization) cannot possibly be approved by an
/// auditor (empty or exceeding `max_diff_bytes`), allowing callers to
/// short-circuit with 0 worktrees and 0 LLM tokens.
pub(super) fn short_circuit_audit_reason_with_limit(
    diff: &str,
    max_diff_bytes: usize,
) -> Option<String> {
    if diff.trim().is_empty() {
        return Some("inconclusive: empty diff: nothing to audit".to_string());
    }
    let prepared = cross_audit::prompt::summarize_diff_lockfiles(diff);
    if cross_audit::prompt::truncate_diff_with_limit(&prepared, max_diff_bytes).1 {
        let top = cross_audit::prompt::top_diff_files(&prepared);
        let hint = if top.is_empty() {
            String::new()
        } else {
            format!(" [largest: {top}]")
        };
        return Some(format!(
            "inconclusive: diff was truncated ({} > {} bytes): the unseen part cannot be approved{hint}",
            prepared.len(),
            max_diff_bytes,
        ));
    }
    None
}

pub(super) fn short_circuit_audit_reason(diff: &str) -> Option<String> {
    short_circuit_audit_reason_with_limit(diff, cross_audit::prompt::MAX_DIFF_BYTES)
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
        "eai_again",
        "ehostunreach",
        "enetunreach",
        "connection refused",
        "connection timed out",
        "connection dropped",
        "can't reach the api server",
        "unable to connect to api",
        "could not resolve host",
        "no internet route",
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
/// Summarizes allowlisted lockfiles before diff truncation and performs up to
/// [`MAX_AUDITOR_INCONCLUSIVE_RETRIES`] (2) format/citation-repair retries when
/// the auditor succeeded with unparseable / schema-invalid output (`Inconclusive`)
/// and budget remains.
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
    let provider = audit_backend.map_or_else(
        || provider_for(&deps.settings.repo.run.executor),
        |a| provider_for(&a.provider),
    );
    let mut run = worker::begin_run_with(
        deps,
        task,
        tree,
        AUDIT_EXECUTOR,
        provider,
        grant,
        hook_required,
        accounts,
    )?;
    run.request.sandbox.read_only = true;
    let prepared_input = AuditInput {
        spec: input.spec.clone(),
        diff: cross_audit::prompt::summarize_diff_lockfiles(&input.diff),
        mobile: input.mobile,
    };
    let max_diff_bytes = deps.settings.repo.cross_audit.max_diff_bytes;
    let watch = InfraWatch::new(runner);
    let tokens_measured = watch.reports_tokens();
    let mut report = match cross_audit::run_scored_audit_with_limit(
        &prepared_input,
        &watch,
        &run.request,
        require_scores,
        max_diff_bytes,
    ) {
        Ok(report) => report,
        Err(err) => {
            worker::end_run(
                deps,
                task,
                &run,
                RunStatus::Failed,
                None,
                (0, 0),
                false,
                false,
            )?;
            return Err(err);
        }
    };
    let (
        mut infra,
        mut limit_hit,
        failed_tail,
        last_succeeded,
        mut any_timed_out,
        mut last_final_text,
    ) = watch.take_all();
    if !any_timed_out
        && limit_hit.is_none()
        && infra.is_none()
        && let Some(tail) = failed_tail.as_deref()
    {
        classify_auditor_log_tail(tail, deps.now(), &mut infra, &mut limit_hit);
    }

    let mut saw_named_blocker = false;
    for _attempt in 0..MAX_AUDITOR_INCONCLUSIVE_RETRIES {
        let reason = match &report.verdict {
            AuditVerdict::Inconclusive { reason }
                if last_succeeded
                    && !report.diff_truncated
                    && infra.is_none()
                    && limit_hit.is_none() =>
            {
                reason.clone()
            }
            _ => break,
        };
        if cross_audit::verdict::names_blocking_yes(&last_final_text, &report.checklist) {
            saw_named_blocker = true;
        }
        let mut projected = worker::ensure_budget(deps, &task.root_id)?;
        projected.add_usage(report.tokens_in.saturating_add(report.tokens_out), 0, 0);
        if projected.check() != BudgetCheck::Ok {
            break;
        }
        let mut repair_req = run.request.clone();
        repair_req.run_id = unblock::unique_id();
        repair_req.prompt = cross_audit::prompt::build_format_repair_prompt_with_limit(
            &prepared_input,
            require_scores,
            &reason,
            &last_final_text,
            max_diff_bytes,
        );
        if repair_req.json_schema.is_none() {
            repair_req.json_schema = Some(cross_audit::scored_audit_json_schema(require_scores));
        }
        // Fall back to the latest Inconclusive verdict if the runner errors
        // on the retry (e.g. transient port failure or single-shot test mock).
        let Ok(repair_out) = runner.run(&repair_req) else {
            break;
        };
        report.tokens_in = report.tokens_in.saturating_add(repair_out.tokens_in);
        report.tokens_out = report.tokens_out.saturating_add(repair_out.tokens_out);
        if let Some(hit) = repair_out.limit_hit {
            limit_hit = Some(hit);
            break;
        }
        if let Some(inf) = repair_out.infra {
            infra = Some(inf);
            break;
        }
        if repair_out.timed_out {
            any_timed_out = true;
            break;
        }
        if !repair_out.succeeded() {
            if !repair_out.log_tail.trim().is_empty() {
                classify_auditor_log_tail(
                    &repair_out.log_tail,
                    deps.now(),
                    &mut infra,
                    &mut limit_hit,
                );
            }
            break;
        }
        let (visible_diff, _) =
            cross_audit::prompt::truncate_diff_with_limit(&prepared_input.diff, max_diff_bytes);
        let hunks = cross_audit::parse_diff_hunks_with_truncation(visible_diff, false);
        let parsed = cross_audit::validate_verdict_against_diff(
            cross_audit::parse_scored_verdict(
                &repair_out.final_text,
                &report.checklist,
                require_scores,
            ),
            &hunks,
        );
        // A blocker an earlier reply named (but left uncited or cited outside
        // the diff) may be cited now, never dropped into a pass.
        if saw_named_blocker && parsed.verdict.is_pass() {
            break;
        }
        if !matches!(parsed.verdict, AuditVerdict::Inconclusive { .. }) {
            report.verdict = parsed.verdict;
            report.scores = parsed.scores;
            break;
        }
        report.verdict = parsed.verdict;
        last_final_text = repair_out.final_text;
    }

    let status = if limit_hit.is_some() {
        RunStatus::LimitHit
    } else if report.verdict.is_pass() {
        RunStatus::Succeeded
    } else {
        RunStatus::Failed
    };
    let tokens = (report.tokens_in, report.tokens_out);
    let charge_wall =
        any_timed_out || (infra.is_none() && limit_hit.is_none()) || tokens.0 > 0 || tokens.1 > 0;
    worker::end_run(
        deps,
        task,
        &run,
        status,
        None,
        tokens,
        tokens_measured,
        charge_wall,
    )?;
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
/// Network failures return [`Next::RetryAfter`]. Auth failures quarantine the
/// failing account and rotate (`Next::Switch`) when another healthy account
/// exists in `accounts`, halting only when none remain.
pub(super) fn halt_on_infra_with(
    deps: &Deps,
    task_id: &str,
    grant: &Grant,
    accounts: &dyn AccountPool,
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
    if infra.kind == InfraKind::Auth && switch_after_auth(deps, task_id, grant, accounts, infra)? {
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

/// O12-R10-2 / Plan v3 auth quarantine: a login refused on one account says
/// nothing about the others: the account is quarantined and another takes the
/// task. False when no other account can (single or pinned account, or all
/// others quarantined/resting), and the run halts.
fn switch_after_auth(
    deps: &Deps,
    task_id: &str,
    grant: &Grant,
    accounts: &dyn AccountPool,
    infra: &InfraFailure,
) -> PortResult<bool> {
    let Some(acct) = &grant.account else {
        return Ok(false);
    };
    if accounts.provider_account_count()? < 2 || accounts.after_limit()? == Some(QuotaRoute::Wait) {
        return Ok(false);
    }
    let now = deps.now();
    let until = now.saturating_add(AUTH_QUARANTINE_SECS);
    let raw = format!("auth: {}", unblock::one_line(&infra.raw, 300));
    let hit = LimitHit {
        reset_at: Some(until),
        raw: raw.clone(),
    };
    accounts.report_limit(&acct.id, &hit)?;
    deps.store.set_meta(
        &format!("account_auth_quarantine:{}", acct.id),
        &until.to_string(),
    )?;
    deps.emit(
        EventKind::AccountCooldown,
        task_id,
        serde_json::json!({
            "account": acct.id,
            "reset_at": Some(until),
            "reason": "auth_quarantine",
            "raw": raw,
        }),
    )?;
    let can_rotate = match accounts.after_limit()? {
        Some(QuotaRoute::Switch) => true,
        Some(QuotaRoute::Wait) => false,
        None => accounts.profiles()?.iter().any(|profile| {
            profile.provider == acct.provider
                && profile.id != acct.id
                && deps
                    .store
                    .get_meta(&format!("account_auth_quarantine:{}", profile.id))
                    .ok()
                    .flatten()
                    .and_then(|v| v.parse::<i64>().ok())
                    .is_none_or(|exp| exp <= now)
        }),
    };
    Ok(can_rotate)
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
    stage: Option<&StageTarget>,
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
            unblock::handle_with(deps, task_id, Some(worktree), signal, grant, stage)
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
