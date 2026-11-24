//! Auditor turn execution (with 1-shot format-repair retry on `Inconclusive`),
//! short-circuit guard for empty/truncated diffs, and runner outcome watcher.

use std::sync::{Mutex, PoisonError};

use aaf_domain::budget::BudgetCheck;
use aaf_domain::ports::{
    AgentOutcome, AgentRequest, AgentRunner, InfraFailure, InfraKind, LimitHit, PortResult,
};
use aaf_domain::run::RunStatus;
use aaf_domain::task::{Task, TaskState};

use super::audit_backend::{AuditBackend, audit_cmd_is_claude};
use super::ports::{AccountPool, Grant};
use super::worker;
use super::{Deps, unblock};
use crate::cross_audit::{self, AUDIT_EXECUTOR, AuditInput, AuditReport, AuditVerdict};
use crate::git::TaskWorktree;

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

fn classify_auditor_log_tail(
    tail: &str,
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
            reset_at: None,
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
        classify_auditor_log_tail(tail, &mut infra, &mut limit_hit);
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
                &report.checklist,
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
                        classify_auditor_log_tail(&repair_out.log_tail, &mut infra, &mut limit_hit);
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
                    if !matches!(parsed.verdict, AuditVerdict::Inconclusive { .. }) {
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
/// Network failures return [`worker::Next::RetryAfter`]; auth failures halt immediately.
pub(super) fn halt_on_infra(
    deps: &Deps,
    task_id: &str,
    grant: &Grant,
    infra: &InfraFailure,
) -> PortResult<worker::Next> {
    let (what, fix) = infra_words(grant, infra);
    let raw = unblock::one_line(&infra.raw, 300);
    deps.move_to(
        task_id,
        TaskState::Ready,
        None,
        &format!("agent CLI failed: {what} ({raw}); not counted as an attempt"),
    )?;
    let detail = halt_message(task_id, what, infra, &fix);
    if infra.kind == InfraKind::Network {
        return Ok(worker::Next::RetryAfter {
            until: deps.now(),
            attempt: 1,
            detail,
        });
    }
    Ok(worker::Next::Halt(detail))
}
