//! Cross-provider adversarial audit backend, policy resolution and command
//! template construction.

use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;

use aaf_domain::budget::BudgetCheck;
use aaf_domain::config::{AuditScope, CrossAuditConfig};
use aaf_domain::event::EventKind;
use aaf_domain::pipeline::is_known_real_reviewer;
use aaf_domain::ports::{AgentOutcome, AgentRunner, LimitHit, PortResult};
use aaf_domain::task::Task;
use serde_json::json;

#[allow(dead_code)]
#[path = "audit_backend_policy.rs"]
mod policy;

pub use super::rework_filter::{findings_from_review_issues, is_valid_rework_finding};
pub use policy::{audit_cmd_is_claude, resolve_audit_policy, same_audit_provider};

/// Effective audit provider name from config (`[cross_audit].executor` when
/// non-empty, `"custom"` when only `executor_cmd` is set, `"claude"` when
/// `executor_cmd` invokes `claude`, otherwise the first real reviewer in
/// `cross_audit.reviewers` with `canonical_provider != run_executor`, then
/// first real reviewer, else `[run].executor`) (R3-PROTO-2).
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
        return trimmed;
    }
    if !custom_cmd.is_empty() {
        return "custom";
    }
    if !cross_audit.reviewers.is_empty() {
        if let Some(diff) = cross_audit
            .reviewers
            .iter()
            .find(|r| is_known_real_reviewer(r) && !same_audit_provider(&r.executor, run_executor))
        {
            return diff.executor.trim();
        }
        if let Some(real) = cross_audit
            .reviewers
            .iter()
            .find(|r| is_known_real_reviewer(r))
        {
            return real.executor.trim();
        }
    }
    run_executor.trim()
}

/// Resolves `[cross_audit].executor = "auto"` once to another installed CLI
/// with a config directory under `home`, keeping `"antigravity"` distinct from
/// `"gemini"`, falling back to `run_executor` if none is ready (R3-PROTO-2).
pub fn resolve_auto_audit_executor(
    run_executor: &str,
    configured: &str,
    path_var: &OsStr,
    home: &Path,
) -> String {
    let trimmed = configured.trim();
    if trimmed.is_empty() {
        return run_executor.trim().to_string();
    }
    if !trimmed.eq_ignore_ascii_case("auto") {
        return trimmed.to_string();
    }
    let has_claude_dir = |h: &Path| {
        h.join(".claude").is_dir()
            || std::fs::read_dir(h).ok().is_some_and(|entries| {
                entries.flatten().any(|e| {
                    e.path().is_dir()
                        && e.file_name()
                            .to_str()
                            .is_some_and(|n| n.starts_with(".claude-"))
                })
            })
    };
    const CANDIDATES: [(&str, &[&str]); 5] = [
        ("gemini", &[".gemini"]),
        ("antigravity", &[".gemini"]),
        ("codex", &[".codex"]),
        ("claude", &[".claude"]),
        ("opencode", &[".local/share/opencode", ".config/opencode"]),
    ];
    for (candidate, dirs) in CANDIDATES {
        if same_audit_provider(run_executor, candidate) {
            continue;
        }
        let installed = crate::executors::registry::find_descriptor(candidate)
            .and_then(|d| crate::executors::registry::resolve_binary(d, path_var))
            .is_some();
        let configured_dir = if candidate == "claude" {
            has_claude_dir(home)
        } else {
            dirs.iter().any(|rel| home.join(rel).is_dir())
        };
        if installed && configured_dir {
            return candidate.to_string();
        }
    }
    run_executor.trim().to_string()
}

/// Builds the `GenericRunner` argv template for non-Claude `[cross_audit]`
/// executors (`codex`, `gemini`, `antigravity`, `opencode`, `custom`) or an
/// explicit `executor_cmd` override.
///
/// Keeps `"antigravity"` separate from `"gemini"`, invoking `agy` via
/// [`crate::executors::registry::build_headless_argv`] (R3-PROTO-2).
pub fn build_audit_command(cfg: &CrossAuditConfig) -> Result<Vec<String>, String> {
    if cfg.executor_cmd.trim().is_empty() && cfg.executor.trim().eq_ignore_ascii_case("antigravity")
    {
        let model = cfg.model.trim();
        let effort = cfg.effort.trim();
        let desc = crate::executors::registry::find_descriptor("antigravity")
            .ok_or_else(|| "antigravity descriptor missing from executor registry".to_string())?;
        return crate::executors::registry::build_headless_argv(
            desc,
            "agy",
            "{prompt}",
            (!model.is_empty()).then_some(model),
            (!effort.is_empty()).then_some(effort),
            true,
        );
    }
    policy::build_audit_command(cfg)
}

pub(crate) use super::audit_turn::run_auditor_turn;
use super::audit_turn::short_circuit_audit_reason;
use super::ports::{AccountPool, Grant};
use super::rework_filter;
use super::worker::{GateJob, Job, Next, PLAN_EXECUTOR, PreGateAudit};
use super::{Deps, unblock};
use crate::cross_audit::{self, AUDIT_EXECUTOR, AuditInput, AuditVerdict};
use crate::unblocker::{FailureSignal, PROPOSER_EXECUTOR, QuotaRoute};

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
            json!({ "account": account, "reset_at": hit.reset_at, "raw": unblock::one_line(&hit.raw, 300) }),
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

/// O12-1: whether an audit of fingerprint `fp` (the `fp_field` of its
/// `CrossAudit` event) that ended without a verdict gets one more run: only
/// a readable, complete diff, and only once per fingerprint (the event of
/// the first one carries `"retry": true`), and only while the root budget
/// still allows work.
pub(crate) fn retry_inconclusive(
    deps: &Deps,
    task: &Task,
    (verdict, diff_truncated): (&AuditVerdict, bool),
    fp_field: &str,
    fp: &str,
) -> PortResult<bool> {
    if diff_truncated || !matches!(verdict, AuditVerdict::Inconclusive { .. }) {
        return Ok(false);
    }
    if super::worker::ensure_budget(deps, &task.root_id)?.check() != BudgetCheck::Ok {
        return Ok(false);
    }
    let retried = deps
        .store
        .events_for_task(&task.id, 200)?
        .iter()
        .filter(|e| e.kind == EventKind::CrossAudit)
        .filter_map(|e| serde_json::from_str::<serde_json::Value>(&e.payload_json).ok())
        .any(|p| p["retry"] == json!(true) && p[fp_field] == json!(fp));
    Ok(!retried)
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
        .find(|r| {
            r.executor != AUDIT_EXECUTOR
                && r.executor != PLAN_EXECUTOR
                && r.executor != PROPOSER_EXECUTOR
        })
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

/// Verifies a real git worktree is clean and returns its current `tree_fp`.
fn check_clean_tree_fp(worktree: &Path) -> Result<String, String> {
    let status = aaf_fp::git(
        worktree,
        &["status", "--porcelain"],
        &[],
        crate::gate::GIT_TIMEOUT,
    )
    .map_err(|e| format!("audit worktree status failed: {e}"))?;
    if !status.trim().is_empty() {
        return Err("audit worktree has uncommitted changes".to_string());
    }
    aaf_fp::tree_fp(worktree, crate::gate::GIT_TIMEOUT)
        .map_err(|e| format!("audit worktree tree_fp failed: {e}"))
}

/// Cross-audit of a task whose locked files changed. A pass signs the approval
/// for the task's own worktree and sends it back to the gate.
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
    let fp_before = if job.worktree.path.join(".git").exists() {
        match check_clean_tree_fp(&job.worktree.path) {
            Ok(fp) => Some(fp),
            Err(why) => {
                unblock::block_untried(deps, &task.id, &why)?;
                return Ok(Next::Blocked);
            }
        }
    } else {
        None
    };
    let cfg = &deps.settings.repo.cross_audit;
    let binding = (cfg.audit_scope != AuditScope::HeadLock || cfg.score_gate)
        .then(|| deps.gate.tree_binding(&job.worktree.path))
        .flatten();
    let input = AuditInput {
        spec: task.spec.clone(),
        diff: deps.git.diff(&job.worktree)?,
        mobile: deps.settings.mobile,
    };
    if let Some(reason) = short_circuit_audit_reason(&input.diff) {
        deps.emit(
            EventKind::CrossAudit,
            &task.id,
            json!({
                "verdict": reason, "provider": audit_provider, "reviewer": reviewer,
                "same_account": same_account, "lock_fp": lock_fp, "short_circuited": true,
            }),
        )?;
        let note = format!("cross-audit rejected the changes to locked files ({reason})");
        deps.store
            .append_context_note(&task.id, &unblock::one_line(&note, 400), deps.now())?;
        let signal = audit_rejection_signal("cross-audit rejected the changes to locked files");
        return Ok(Next::Unblock {
            task_id: task.id,
            worktree: job.worktree,
            signal: Box::new(signal),
        });
    }

    // Auditor runs in a clean base tree so it never loads the author's CLAUDE.md/hooks.
    let clean = deps.git.integrate(&[])?;
    let audited = run_auditor_turn(
        deps,
        audit_backend,
        &task,
        &input,
        &clean.tree,
        grant,
        cfg.score_gate,
    );
    let _ = deps.git.discard(&clean);
    let (report, infra, limit_hit) = audited?;
    if let Some(hit) = limit_hit {
        let accounts: &dyn AccountPool = audit_backend.map_or(&*deps.accounts, |a| &*a.accounts);
        report_audit_limit(deps, accounts, &task.id, grant, &hit)?;
        return Ok(match route_audit_limit(accounts, grant, &hit)? {
            Next::Switch => Next::SwitchJob(Box::new(Job::Audit {
                job,
                lock_fp: lock_fp.to_string(),
            })),
            other => other,
        });
    }
    if let Some(infra) = infra {
        return super::post_audit::audit_infra_next(deps, &task.id, grant, &infra);
    }

    let approval = if let Some(ref before) = fp_before {
        match check_clean_tree_fp(&job.worktree.path) {
            Ok(ref after) if after == before => {}
            Ok(_) => {
                unblock::block_untried(
                    deps,
                    &task.id,
                    "audit worktree tree_fp changed during cross-audit",
                )?;
                return Ok(Next::Blocked);
            }
            Err(why) => {
                unblock::block_untried(deps, &task.id, &why)?;
                return Ok(Next::Blocked);
            }
        }
        cross_audit::approve_if_pass_with_config(
            &report,
            &deps.settings.data_dir,
            &job.worktree.path,
            &deps.settings.repo,
            lock_fp,
            &reviewer,
            same_account,
            &*deps.clock,
        )?
    } else {
        cross_audit::approve_if_pass(
            &report,
            &deps.settings.data_dir,
            &job.worktree.path,
            lock_fp,
            &reviewer,
            same_account,
            &*deps.clock,
        )?
    };
    let text = verdict_text(&report.verdict);
    let retry = approval.is_none()
        && retry_inconclusive(
            deps,
            &task,
            (&report.verdict, report.diff_truncated),
            "lock_fp",
            lock_fp,
        )?;
    deps.emit(
        EventKind::CrossAudit,
        &task.id,
        json!({
            "verdict": text,
            "provider": audit_provider,
            "reviewer": reviewer,
            "same_account": same_account,
            "lock_fp": lock_fp,
            "retry": retry,
        }),
    )?;
    if retry {
        let lock_fp = lock_fp.to_string();
        return Ok(Next::RetryAudit(Box::new(Job::Audit { job, lock_fp })));
    }
    if approval.is_some() {
        let next_job = GateJob {
            audited: true,
            cached_receipt: None,
            ..job
        };
        if let Some((tree_fp, base_sha)) = binding {
            let pre = PreGateAudit {
                tree_fp,
                base_sha,
                audit_provider,
                reviewer,
                same_account,
                scores: report.scores,
            };
            return Ok(Next::GateAudited(next_job, Box::new(pre)));
        }
        return Ok(Next::Gate(next_job));
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
    let signal = audit_rejection_signal("cross-audit rejected the changes to locked files");
    Ok(Next::Unblock {
        task_id: task.id,
        worktree: job.worktree,
        signal: Box::new(signal),
    })
}
