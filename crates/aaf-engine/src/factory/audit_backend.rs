//! Cross-provider adversarial audit backend, policy resolution and command
//! template construction.

use std::path::Path;
use std::sync::Arc;

use aaf_domain::config::CrossAuditConfig;
use aaf_domain::event::EventKind;
use aaf_domain::ports::{AgentOutcome, AgentRunner, InfraFailure, InfraKind, LimitHit, PortResult};
use aaf_domain::run::RunStatus;
use aaf_domain::task::Task;
use serde_json::json;

use super::ports::{AccountPool, Grant};
use super::rework_filter;
use super::unblock::InfraWatch;
use super::worker::{self, GateJob, Next, PLAN_EXECUTOR};
use super::{Deps, unblock};
use crate::cross_audit::{self, AUDIT_EXECUTOR, AuditInput, AuditReport, AuditVerdict};
use crate::git::TaskWorktree;
use crate::unblocker::{FailureSignal, QuotaRoute};

const PROMPT_PLACEHOLDER: &str = "{prompt}";

/// Dedicated auditor backend built from `[cross_audit]`.
///
/// Runs the adversarial audit in the same clean base worktree and Seatbelt
/// sandbox as the default auditor, using an independent provider/runner and
/// provider-scoped account pool.
pub struct AuditBackend {
    pub provider: String,
    pub runner: Arc<dyn AgentRunner>,
    pub accounts: Arc<dyn AccountPool>,
    pub hook_required: bool,
}

fn canonical_provider(provider: &str) -> String {
    match provider.trim().to_ascii_lowercase().as_str() {
        "antigravity" | "gemini" => "gemini".to_string(),
        other => other.to_string(),
    }
}

/// True when two provider names refer to the same underlying provider
/// (e.g. `gemini` and `antigravity`).
pub fn same_audit_provider(a: &str, b: &str) -> bool {
    canonical_provider(a) == canonical_provider(b)
}

/// True when `executor_cmd` invokes a `claude` binary as `argv[0]`.
pub fn audit_cmd_is_claude(executor_cmd: &str) -> bool {
    let Ok(words) = split_cmd_words(executor_cmd.trim()) else {
        return false;
    };
    let Some(arg0) = words.first() else {
        return false;
    };
    Path::new(arg0)
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("claude"))
}

/// Effective audit provider name from config (`[cross_audit].executor` when
/// non-empty, `"custom"` when only `executor_cmd` is set, `"claude"` when
/// `executor_cmd` invokes `claude`, otherwise `[run].executor`).
pub fn configured_audit_provider<'a>(
    run_executor: &'a str,
    cross_audit: &'a CrossAuditConfig,
) -> &'a str {
    let custom_cmd = cross_audit.executor_cmd.trim();
    if !custom_cmd.is_empty() && audit_cmd_is_claude(custom_cmd) {
        return "claude";
    }
    let trimmed = cross_audit.executor.trim();
    if !trimmed.is_empty() {
        trimmed
    } else if !custom_cmd.is_empty() {
        "custom"
    } else {
        run_executor.trim()
    }
}

/// Effective audit provider for a running factory loop.
pub fn effective_audit_provider<'a>(
    run_executor: &'a str,
    audit: Option<&'a AuditBackend>,
) -> &'a str {
    match audit {
        Some(backend) if !backend.provider.trim().is_empty() => backend.provider.trim(),
        _ => run_executor.trim(),
    }
}

/// Computes `(reviewer, same_account)` for an audit approval and enforces
/// `require_different_provider` (fail-closed when the auditor shares the
/// author's provider).
pub fn resolve_audit_policy(
    run_provider: &str,
    author_account: Option<&str>,
    audit_provider: &str,
    auditor_account: Option<&str>,
    require_different_provider: bool,
) -> Result<(String, bool), String> {
    let run_p = run_provider.trim();
    let audit_p = audit_provider.trim();
    let run_canon = canonical_provider(run_p);
    let audit_canon = canonical_provider(audit_p);
    let different_provider = audit_canon != run_canon;
    if !different_provider && require_different_provider {
        return Err(format!(
            "cross_audit.require_different_provider is true, but auditor uses the same provider \
             ({audit_p}) as [run].executor ({run_p}); configure a different [cross_audit].executor"
        ));
    }
    let same_account = if audit_canon == "custom" {
        // Unverified custom commands may invoke any underlying account/CLI.
        true
    } else if different_provider {
        false
    } else {
        // Unknown on either side counts as the same account: never overclaim
        // independence within the same provider.
        match (author_account, auditor_account) {
            (Some(author), Some(auditor)) => author == auditor,
            _ => true,
        }
    };
    let reviewer = format!(
        "cross-audit:{audit_p}/{}",
        auditor_account.unwrap_or("default")
    );
    Ok((reviewer, same_account))
}

fn validate_cli_value(field: &str, value: &str) -> Result<(), String> {
    if value.starts_with('-') || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!(
            "invalid cross_audit.{field} {value:?}: must not start with '-' or contain whitespace or control chars"
        ));
    }
    Ok(())
}

/// Builds the `GenericRunner` argv template for non-Claude `[cross_audit]`
/// executors (`codex`, `gemini`, `antigravity`, `opencode`, `custom`) or an
/// explicit `executor_cmd` override.
pub fn build_audit_command(cfg: &CrossAuditConfig) -> Result<Vec<String>, String> {
    let custom = cfg.executor_cmd.trim();
    if !custom.is_empty() {
        let mut words = split_cmd_words(custom)?;
        if words.is_empty() {
            return Err("cross_audit.executor_cmd is empty".to_string());
        }
        let has_placeholder = words[1..]
            .iter()
            .any(|arg| arg.contains(PROMPT_PLACEHOLDER));
        if !has_placeholder {
            // `GenericRunner` passes the prompt as an argv element (no stdin):
            // replace a trailing `-` placeholder or append `{prompt}`.
            if words.len() > 1
                && let Some(last) = words.last_mut()
                && last == "-"
            {
                *last = PROMPT_PLACEHOLDER.to_string();
            } else {
                words.push(PROMPT_PLACEHOLDER.to_string());
            }
        }
        return Ok(words);
    }

    let exec = cfg.executor.trim().to_ascii_lowercase();
    let model = cfg.model.trim();
    let effort = cfg.effort.trim();
    if !model.is_empty() {
        validate_cli_value("model", model)?;
    }
    if !effort.is_empty() {
        validate_cli_value("effort", effort)?;
    }
    match exec.as_str() {
        "codex" => {
            let mut cmd = vec!["codex".to_string(), "exec".to_string()];
            if !model.is_empty() {
                cmd.push("-m".to_string());
                cmd.push(model.to_string());
            }
            if !effort.is_empty() {
                cmd.push("-c".to_string());
                cmd.push(format!("model_reasoning_effort={effort}"));
            }
            cmd.push(PROMPT_PLACEHOLDER.to_string());
            Ok(cmd)
        }
        "gemini" | "antigravity" => {
            let mut cmd = vec!["gemini".to_string()];
            if !model.is_empty() {
                cmd.push("-m".to_string());
                cmd.push(model.to_string());
            }
            cmd.push("-p".to_string());
            cmd.push(PROMPT_PLACEHOLDER.to_string());
            Ok(cmd)
        }
        "opencode" => {
            let mut cmd = vec!["opencode".to_string(), "run".to_string()];
            if !model.is_empty() {
                cmd.push("-m".to_string());
                cmd.push(model.to_string());
            }
            cmd.push(PROMPT_PLACEHOLDER.to_string());
            Ok(cmd)
        }
        "custom" => {
            Err("cross_audit.executor = \"custom\" requires cross_audit.executor_cmd".to_string())
        }
        other => Err(format!(
            "unknown cross_audit.executor {other:?}: expected claude, codex, gemini, antigravity, opencode or custom"
        )),
    }
}

/// Minimal quote-aware argv splitter (no shell expansion).
fn split_cmd_words(input: &str) -> Result<Vec<String>, String> {
    let (mut words, mut cur, mut in_word, mut quote) = (Vec::new(), String::new(), false, '\0');
    let mut chars = input.chars();
    let trail = || "trailing backslash in cross_audit.executor_cmd".to_string();
    while let Some(c) = chars.next() {
        match quote {
            '\'' if c == '\'' => quote = '\0',
            '\'' => cur.push(c),
            '"' if c == '"' => quote = '\0',
            '"' if c == '\\' => cur.push(chars.next().ok_or_else(trail)?),
            '"' => cur.push(c),
            _ => match c {
                '\'' | '"' => (quote, in_word) = (c, true),
                '\\' => {
                    cur.push(chars.next().ok_or_else(trail)?);
                    in_word = true;
                }
                ch if ch.is_whitespace() => {
                    if in_word {
                        words.push(std::mem::take(&mut cur));
                        in_word = false;
                    }
                }
                other => {
                    cur.push(other);
                    in_word = true;
                }
            },
        }
    }
    if quote != '\0' {
        return Err("unterminated quote in cross_audit.executor_cmd".to_string());
    }
    if in_word {
        words.push(cur);
    }
    Ok(words)
}

/// Records an auditor account rate-limit cooldown on the auditor's own pool.
pub(crate) fn report_audit_limit(
    deps: &Deps,
    accounts: &dyn AccountPool,
    task_id: &str,
    grant: &Grant,
    hit: &LimitHit,
) -> PortResult<()> {
    if let Some(account) = grant.account_id() {
        accounts.report_limit(account, hit)?;
        deps.emit(
            EventKind::AccountCooldown,
            task_id,
            json!({
                "account": account,
                "reset_at": hit.reset_at,
                "raw": unblock::one_line(&hit.raw, 300),
            }),
        )?;
    }
    Ok(())
}

/// Decides whether another auditor account may take over after a limit hit.
pub(crate) fn route_audit_limit(
    accounts: &dyn AccountPool,
    grant: &Grant,
    hit: &LimitHit,
) -> PortResult<Next> {
    let wait = Ok(Next::Wait {
        until: hit.reset_at,
    });
    if grant.account_id().is_none() {
        return wait;
    }
    match accounts.after_limit()? {
        Some(QuotaRoute::Switch) => Ok(Next::Switch),
        None if accounts.provider_account_count()? > 1 => Ok(Next::Switch),
        Some(QuotaRoute::Wait) | None => wait,
    }
}

/// Shared failure signal for an audit rejection handed to `Next::Unblock`.
pub(crate) fn audit_rejection_signal(log_tail: &str) -> FailureSignal {
    let mut signal = FailureSignal::empty();
    signal.outcome = Some(AgentOutcome {
        exit_code: Some(1),
        timed_out: false,
        final_text: String::new(),
        tokens_in: 0,
        tokens_out: 0,
        limit_hit: None,
        infra: None,
        log_tail: log_tail.to_string(),
    });
    signal
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

pub(crate) fn verdict_text(verdict: &AuditVerdict) -> String {
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

/// Resolves `(audit_provider, reviewer, same_account)` for `task_id` and
/// enforces `cross_audit.require_different_provider`.
pub(crate) fn resolve_task_audit_context(
    deps: &Deps,
    audit_backend: Option<&AuditBackend>,
    task_id: &str,
    grant: &Grant,
) -> PortResult<Result<(String, String, bool), String>> {
    let author = deps
        .store
        .runs_for_task(task_id)?
        .into_iter()
        .rev()
        .find(|run| run.executor != AUDIT_EXECUTOR && run.executor != PLAN_EXECUTOR)
        .and_then(|run| run.account_id);
    let run_provider = &deps.settings.repo.run.executor;
    let custom_cmd = deps.settings.repo.cross_audit.executor_cmd.trim();
    let audit_provider = if !custom_cmd.is_empty() && audit_cmd_is_claude(custom_cmd) {
        "claude"
    } else {
        effective_audit_provider(run_provider, audit_backend)
    };
    Ok(resolve_audit_policy(
        run_provider,
        author.as_deref(),
        audit_provider,
        grant.account_id(),
        deps.settings.repo.cross_audit.require_different_provider,
    )
    .map(|(reviewer, same_account)| (audit_provider.to_string(), reviewer, same_account)))
}

/// One recorded auditor run in `tree`, plus any infra failure or rate limit.
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
    let run = worker::begin_run_with(
        deps,
        task,
        tree,
        AUDIT_EXECUTOR,
        grant,
        hook_required,
        accounts,
    )?;
    let watch = InfraWatch::new(runner);
    let report = match cross_audit::run_scored_audit(input, &watch, &run.request, require_scores) {
        Ok(report) => report,
        Err(err) => {
            worker::end_run(deps, task, &run, RunStatus::Failed, None, (0, 0), false)?;
            return Err(err);
        }
    };
    let tokens_measured = watch.reports_tokens();
    let (mut infra, mut limit_hit, failed_tail) = watch.take_with_tail();
    if limit_hit.is_none()
        && infra.is_none()
        && let Some(tail) = failed_tail.as_deref()
    {
        classify_auditor_log_tail(tail, &mut infra, &mut limit_hit);
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

/// Cross-audit of a task whose locked files changed. A pass signs the
/// approval for the task's own worktree (approvals are per worktree, and
/// that is where its gate runs) and sends it back to the gate.
pub(crate) fn execute_audit(
    deps: &Deps,
    audit_backend: Option<&AuditBackend>,
    job: GateJob,
    lock_fp: &str,
    grant: &Grant,
) -> PortResult<Next> {
    let task = deps.store.get_task(&job.task_id)?;
    let (audit_provider, reviewer, same_account) =
        match resolve_task_audit_context(deps, audit_backend, &task.id, grant)? {
            Ok(ctx) => ctx,
            Err(why) => {
                unblock::block_untried(deps, &task.id, &why)?;
                return Ok(Next::Blocked);
            }
        };
    let input = AuditInput {
        spec: task.spec.clone(),
        diff: deps.git.diff(&job.worktree)?,
        mobile: deps.settings.mobile,
    };

    // The auditor gets the changes as text in its prompt and works in a
    // clean tree of the base. Started in the author's worktree it would load
    // the author's `CLAUDE.md` and `.claude/` settings (instructions, hooks,
    // env): the very files it may be auditing.
    let clean = deps.git.integrate(&[])?;
    let audited = run_auditor_turn(
        deps,
        audit_backend,
        &task,
        &input,
        &clean.tree,
        grant,
        false,
    );
    let _ = deps.git.discard(&clean);
    let (report, infra, limit_hit) = audited?;
    if let Some(hit) = limit_hit {
        let accounts: &dyn AccountPool = audit_backend.map_or(&*deps.accounts, |a| &*a.accounts);
        report_audit_limit(deps, accounts, &task.id, grant, &hit)?;
        return route_audit_limit(accounts, grant, &hit);
    }
    if let Some(infra) = infra {
        let (what, fix) = worker::infra_words(grant, &infra);
        return Ok(Next::Halt(worker::halt_message(
            &task.id, what, &infra, &fix,
        )));
    }

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
        json!({
            "verdict": text,
            "provider": audit_provider,
            "reviewer": reviewer,
            "same_account": same_account,
            "lock_fp": lock_fp,
        }),
    )?;
    if approval.is_some() {
        return Ok(Next::Gate(GateJob {
            audited: true,
            ..job
        }));
    }
    if !report.diff_truncated
        && let AuditVerdict::Fail { findings, .. } = &report.verdict
        && let Some(next) = unblock::handle_audit_failure(deps, &task, findings)?
    {
        return Ok(next);
    }
    let summary = rework_filter::fallback_verdict_summary(&report.verdict, &text);
    let note = format!("cross-audit rejected the changes to locked files ({summary})");
    deps.store
        .append_context_note(&task.id, &unblock::one_line(&note, 400), deps.now())?;
    // Fixed verdict kind only so finding texts do not create a new failure digest every run.
    let signal = audit_rejection_signal("cross-audit rejected the changes to locked files");
    Ok(Next::Unblock {
        task_id: task.id,
        worktree: job.worktree,
        signal: Box::new(signal),
    })
}
