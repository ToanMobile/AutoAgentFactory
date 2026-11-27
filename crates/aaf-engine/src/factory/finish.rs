//! What happens to a task once the gate has judged it: a pull request (the
//! default), the merge queue (`--auto-merge`), or the way back through the
//! unblocker. Runs on the gate thread, so merges of one repository are
//! naturally one at a time.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use aaf_domain::blocked::UnblockTier;
use aaf_domain::config::{AuditScope, LandMode};
use aaf_domain::event::EventKind;
use aaf_domain::gate::{GateStatus, LockStatus, Receipt, Severity, StoreStatus};
use aaf_domain::ports::PortResult;
use aaf_domain::task::TaskState;
use serde_json::json;

use super::batch::{Group, Judged, Member, Verdict, gate_batch, gate_one};
pub(crate) use super::finish_land::close;
use super::finish_land::{land_branch, land_pr_merge, merge, open_pr};
use super::post_audit;
use super::worker::GateJob;
use super::{Deps, unblock};
use crate::cross_audit::compute_mechanical_score;
use crate::gate::{
    GIT_TIMEOUT, REASON_TREE_CHANGED, docs_fast::DOCS_FAST_PATH_NOTE, runner::THROTTLE_NOTE,
};
use crate::unblocker::ladder;
use crate::unblocker::{FailureSignal, failure_digest, is_pure_test_failure};

mod transient;
use transient::{is_config_error, is_transient_oracle, retry_transient};

/// Written by the gate thread before it reports a task, read by the
/// scheduler when the loop ends.
#[derive(Default)]
pub(super) struct GateStats {
    pub(super) runs: AtomicU32,
    pub(super) warnings: Mutex<Vec<String>>,
}

/// One gate round: judges `jobs` with as few gate runs as possible and hands
/// every job's resolution to `answer` as soon as it is ready.
pub(super) fn judge_round(
    deps: &Deps,
    jobs: Vec<GateJob>,
    stats: &GateStats,
    answer: &dyn Fn(GateJob, Result<Resolution, String>),
) {
    let always_solo = deps.settings.repo.proof.required
        || deps.settings.repo.cross_audit.score_gate
        || deps.settings.repo.cross_audit.dual_review
        || deps.settings.repo.cross_audit.audit_scope == AuditScope::Always;
    let large_scope =
        !always_solo && deps.settings.repo.cross_audit.audit_scope == AuditScope::Large;
    let mut cached_ready: Vec<(GateJob, Receipt)> = Vec::new();
    let mut batch: Vec<GateJob> = Vec::new();
    let mut alone: Vec<GateJob> = Vec::new();
    for mut job in jobs {
        if let Some(receipt) = job.cached_receipt.take()
            && receipt.status == GateStatus::Clear
            && deps
                .gate
                .verify_cached_receipt(&job.worktree.path, &receipt)
        {
            cached_ready.push((job, *receipt));
            continue;
        }
        match deps.gate.head_lock(&job.worktree.path) {
            Ok(lock) if lock.status == LockStatus::Clear => {
                // Proof artifacts and post-gate audit verdicts are bound to
                // each task worktree's own tree_fp; never share a batch receipt.
                let needs_solo = always_solo
                    || (large_scope && post_audit::is_large_worktree_diff(deps, &job.worktree));
                if needs_solo {
                    alone.push(job);
                } else {
                    batch.push(job);
                }
            }
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

    if !cached_ready.is_empty() {
        let mut round = Round::solo(stats);
        let land = deps.settings.effective_land();
        for (job, receipt) in cached_ready {
            let covered = (land == LandMode::Auto && !deps.settings.repo.proof.required)
                .then(|| receipt.base_sha.clone())
                .flatten();
            let judged = Judged {
                task_id: job.task_id.clone(),
                verdict: Verdict::Clear(receipt),
                batch_group: None,
            };
            let res = settle(deps, &job, judged, covered, &mut round).map_err(|e| e.to_string());
            answer(job, res);
        }
    }
    if batch.len() == 1 {
        alone.insert(0, batch.remove(0));
    }
    if !batch.is_empty() {
        let members: Vec<Member> = batch
            .iter()
            .map(|j| Member {
                task_id: j.task_id.clone(),
                worktree: j.worktree.clone(),
            })
            .collect();
        let mut outcome = gate_batch(&*deps.git, &*deps.gate, &members);
        stats.runs.fetch_add(outcome.gate_runs, Ordering::Relaxed);
        if let Ok(mut warnings) = stats.warnings.lock() {
            warnings.append(&mut outcome.warnings);
        }
        let mut round = Round {
            groups: std::mem::take(&mut outcome.groups),
            gate_runs: &stats.runs,
            warnings: &stats.warnings,
        };
        for job in batch {
            let Some(at) = outcome
                .verdicts
                .iter()
                .position(|j| j.task_id == job.task_id)
            else {
                answer(job, Err("the gate returned no verdict".to_string()));
                continue;
            };
            let judged = outcome.verdicts.swap_remove(at);
            let res = settle(deps, &job, judged, None, &mut round).map_err(|e| e.to_string());
            answer(job, res);
        }
    }
    if !deps.settings.effective_land().auto_lands() && alone.len() > 1 {
        std::thread::scope(|scope| {
            let (tx, rx) = std::sync::mpsc::channel();
            for job in alone {
                let tx = tx.clone();
                scope.spawn(move || {
                    let res = run_solo_job(deps, &job, stats);
                    let _ = tx.send((job, res));
                });
            }
            drop(tx);
            for (job, res) in rx {
                answer(job, res);
            }
        });
    } else {
        for job in alone {
            let res = run_solo_job(deps, &job, stats);
            answer(job, res);
        }
    }
}

enum PreRebase {
    Ready(Option<String>),
    Done(Result<Resolution, String>),
}

pub(super) fn push_warning(warnings: &Mutex<Vec<String>>, msg: String) {
    if let Ok(mut list) = warnings.lock() {
        list.push(msg);
    }
}

pub(super) fn conflict_resolution(
    deps: &Deps,
    task_id: &str,
    files: Vec<String>,
) -> PortResult<Resolution> {
    deps.emit(
        EventKind::MergeResult,
        task_id,
        json!({ "merged": false, "conflict": files }),
    )?;
    let mut signal = FailureSignal::empty();
    signal.merge_conflict_files = files;
    Ok(Resolution::Failed(Box::new(signal)))
}

/// Rebases a solo job onto the current base before gating in `LandMode::Auto`
/// (skipped when `proof.required`, which binds commit SHAs to the signed proof).
fn pre_gate_rebase(deps: &Deps, job: &GateJob) -> PreRebase {
    if deps.settings.effective_land() != LandMode::Auto || deps.settings.repo.proof.required {
        return PreRebase::Ready(None);
    }
    let base_before = deps.git.base_sha().ok();
    match deps.git.rebase(&job.worktree) {
        Ok(files) if files.is_empty() => PreRebase::Ready(
            base_before.filter(|before| deps.git.base_sha().ok().as_ref() == Some(before)),
        ),
        Ok(files) => PreRebase::Done(
            conflict_resolution(deps, &job.task_id, files).map_err(|e| e.to_string()),
        ),
        Err(err) => PreRebase::Done(Err(err.to_string())),
    }
}

fn run_solo_job(deps: &Deps, job: &GateJob, stats: &GateStats) -> Result<Resolution, String> {
    let covered = match pre_gate_rebase(deps, job) {
        PreRebase::Ready(covered) => covered,
        PreRebase::Done(res) => return res,
    };
    stats.runs.fetch_add(1, Ordering::Relaxed);
    let judged = Judged {
        task_id: job.task_id.clone(),
        verdict: gate_one(&*deps.gate, &job.worktree.path),
        batch_group: None,
    };
    let mut round = Round::solo(stats);
    settle(deps, job, judged, covered, &mut round).map_err(|e| e.to_string())
}

pub(crate) enum Resolution {
    Settled(TaskState),
    NeedsAudit { lock_fp: String },
    NeedsPostAudit { receipt: Box<Receipt> },
    Failed(Box<FailureSignal>),
    Retry,
}

/// State shared by the tasks of one gate round.
pub(crate) struct Round<'a> {
    /// Gated integration trees of this round. `Group::base` tracks the covered
    /// base along a group's merge chain and clears once the chain breaks.
    pub groups: HashMap<u32, Group>,
    pub gate_runs: &'a AtomicU32,
    /// Non-blocking warnings reported to `aaf run`.
    pub warnings: &'a Mutex<Vec<String>>,
}

impl<'a> Round<'a> {
    fn solo(stats: &'a GateStats) -> Self {
        Self {
            groups: HashMap::new(),
            gate_runs: &stats.runs,
            warnings: &stats.warnings,
        }
    }
}

pub(super) fn receipt_duration_ms(receipt: &Receipt) -> u64 {
    receipt
        .commands
        .iter()
        .fold(0u64, |acc, c| acc.saturating_add(c.duration_ms))
}

pub(super) fn gate_payload(
    deps: &Deps,
    worktree: &Path,
    receipt: &Receipt,
    batch_group: Option<u32>,
) -> serde_json::Value {
    let sev = |s: Severity| {
        receipt
            .static_findings
            .iter()
            .filter(|f| f.severity == s)
            .count()
    };
    let passed = receipt.commands.iter().filter(|c| c.passed()).count();
    let duration_ms = receipt_duration_ms(receipt);
    let proof_ok = post_audit::proof_verified(deps, worktree, receipt);
    let score = compute_mechanical_score(receipt, proof_ok, false, false).score;
    json!({
        "status": receipt.status, "reasons": receipt.reasons, "tree_fp": receipt.tree_fp,
        "config_hash": receipt.config_hash, "gated_tree": receipt.repo_root,
        "batched": batch_group.is_some(), "batch_group": batch_group,
        "static_warnings": sev(Severity::Warn), "static_critical": sev(Severity::Critical),
        "store_status": receipt.store_compliance.status,
        "store_warnings": receipt.store_compliance.warnings,
        "store_critical": receipt.store_compliance.critical,
        "store_unresolved": receipt.store_compliance.unresolved.len(),
        "head_lock_status": receipt.head_lock.status, "suppressions": receipt.suppressions,
        "commands_passed": passed, "commands_total": receipt.commands.len(),
        "duration_ms": duration_ms,
        "mechanical_score": score,
    })
}

pub(super) fn is_tree_changed_unverified(receipt: &Receipt) -> bool {
    receipt.status == GateStatus::Unverified
        && receipt
            .reasons
            .iter()
            .any(|r| r.contains(REASON_TREE_CHANGED))
}

pub(super) fn tree_changed_failure(
    deps: &Deps,
    task_id: &str,
    worktree: &Path,
    receipt: Receipt,
) -> PortResult<Resolution> {
    // Route `tree changed during gate` to retryable T3 with a dirty-file
    // diagnostic; never auto-commit unsandboxed gate side effects.
    if worktree.join(".git").exists()
        && let Ok(status) = aaf_fp::git(worktree, &["status", "--porcelain"], &[], GIT_TIMEOUT)
    {
        let dirty: Vec<&str> = status
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .take(10)
            .collect();
        if !dirty.is_empty() {
            let note = format!(
                "tree changed during gate; dirty files: {}",
                dirty.join(", ")
            );
            deps.store
                .append_context_note(task_id, &unblock::one_line(&note, 300), deps.now())?;
        }
    }
    let mut signal = FailureSignal::empty();
    signal.gate = Some(receipt);
    Ok(Resolution::Failed(Box::new(signal)))
}

pub(super) fn is_head_lock_only_block(receipt: &Receipt) -> bool {
    if receipt.status != GateStatus::Blocked
        || receipt.head_lock.status != LockStatus::Blocked
        || receipt.head_lock.lock_fp.is_empty()
        || receipt.store_compliance.status == StoreStatus::Blocked
        || receipt
            .static_findings
            .iter()
            .any(|f| f.severity == Severity::Critical)
        || receipt.commands.iter().any(|c| !c.passed())
    {
        return false;
    }
    !receipt.reasons.iter().any(|r| {
        !(r.starts_with("head lock:")
            || r.starts_with("commands skipped:")
            || r.starts_with("warning:")
            || r.starts_with(THROTTLE_NOTE)
            || r.starts_with(DOCS_FAST_PATH_NOTE)
            || r.starts_with("work tree: removed "))
    })
}

fn is_proof_only_block(receipt: &Receipt) -> bool {
    if receipt.status != GateStatus::Blocked
        || receipt.head_lock.status == LockStatus::Blocked
        || receipt.store_compliance.status == StoreStatus::Blocked
        || receipt
            .static_findings
            .iter()
            .any(|f| f.severity == Severity::Critical)
        || receipt.commands.iter().any(|c| !c.passed())
        || !receipt
            .reasons
            .iter()
            .any(|r| r.starts_with("proof required:"))
    {
        return false;
    }
    !receipt.reasons.iter().any(|r| {
        !(r.starts_with("proof required:")
            || r.starts_with("commands skipped:")
            || r.starts_with("warning:")
            || r.starts_with(THROTTLE_NOTE)
            || r.starts_with(DOCS_FAST_PATH_NOTE)
            || r.starts_with("work tree: removed "))
    })
}

fn has_host_proof_automation(cfg: &aaf_domain::config::ProofConfig) -> bool {
    !cfg.capture_cmd.trim().is_empty() || !cfg.flows.is_empty()
}

fn already_emitted_clear_gate(deps: &Deps, task_id: &str, receipt: &Receipt) -> bool {
    if receipt.tree_fp.is_empty() {
        return false;
    }
    let Ok(events) = deps
        .store
        .events_for_task_by_kinds(task_id, &[EventKind::GateResult], 1)
    else {
        return false;
    };
    let Some(last) = events.first() else {
        return false;
    };
    let Ok(val) = serde_json::from_str::<serde_json::Value>(&last.payload_json) else {
        return false;
    };
    val.get("status").and_then(|v| v.as_str()) == Some("CLEAR")
        && val.get("tree_fp").and_then(|v| v.as_str()) == Some(receipt.tree_fp.as_str())
        && val.get("config_hash").and_then(|v| v.as_str()) == Some(receipt.config_hash.as_str())
}

pub(crate) fn settle(
    deps: &Deps,
    job: &GateJob,
    judged: Judged,
    covered_base: Option<String>,
    round: &mut Round<'_>,
) -> PortResult<Resolution> {
    let task = deps.store.get_task(&job.task_id)?;
    let blocked = |why: String| -> PortResult<Resolution> {
        unblock::block_untried(deps, &task.id, &why)?;
        Ok(Resolution::Settled(TaskState::Blocked))
    };
    let emit_gate = |receipt: &Receipt| {
        deps.emit(
            EventKind::GateResult,
            &task.id,
            gate_payload(deps, &job.worktree.path, receipt, judged.batch_group),
        )
    };
    match judged.verdict {
        Verdict::Clear(receipt) => {
            let needs_audit = post_audit::needs_post_gate_audit(deps, job, &receipt);
            if !(needs_audit && already_emitted_clear_gate(deps, &task.id, &receipt)) {
                emit_gate(&receipt)?;
            }
            if needs_audit {
                return Ok(Resolution::NeedsPostAudit {
                    receipt: Box::new(receipt),
                });
            }
            let gated_in = judged.batch_group.and_then(|id| round.groups.get(&id));
            if let Some(why) = crate::proof::verified_proof_needs_human_reason(
                &job.worktree.path,
                &deps.settings.data_dir,
                &deps.settings.repo.proof,
                &receipt.tree_fp,
            ) {
                deps.store.append_context_note(&task.id, &why, deps.now())?;
                return match super::full_gate::land_for(deps, &task) {
                    LandMode::Pr | LandMode::PrMerge => {
                        open_pr(deps, job, &task, &receipt, gated_in, round.warnings)
                    }
                    LandMode::Auto | LandMode::Branch => {
                        deps.move_to(&task.id, TaskState::Review, None, &why)?;
                        unblock::root_succeeded(deps, &task)?;
                        Ok(Resolution::Settled(TaskState::Review))
                    }
                };
            }
            match super::full_gate::land_for(deps, &task) {
                LandMode::Branch => land_branch(deps, job, &task),
                LandMode::Pr => open_pr(deps, job, &task, &receipt, gated_in, round.warnings),
                LandMode::PrMerge => {
                    land_pr_merge(deps, job, &task, &receipt, gated_in, round.warnings)
                }
                LandMode::Auto => merge(
                    deps,
                    job,
                    &task,
                    &receipt,
                    judged.batch_group,
                    covered_base,
                    round,
                ),
            }
        }
        Verdict::Red(receipt) => {
            emit_gate(&receipt)?;
            if is_proof_only_block(&receipt)
                && !has_host_proof_automation(&deps.settings.repo.proof)
            {
                return blocked(format!(
                    "missing signed device proof (host/operator must run `aaf proof` or configure \
                     `[proof].capture_cmd` / `[[proof.flows]]`): {}",
                    receipt.reasons.join("; ")
                ));
            }
            let mut signal = FailureSignal::empty();
            signal.gate = Some(receipt);
            let digest = failure_digest(&signal);
            let rerun_eligible = judged.batch_group.is_none()
                && signal.gate.as_ref().is_some_and(is_pure_test_failure)
                && !ladder::tried(&task, UnblockTier::T2Mechanical, &digest);
            if rerun_eligible && let Some(receipt) = signal.gate.as_ref() {
                match deps.gate.rerun_failed_test(&job.worktree.path, receipt) {
                    Ok(rerun) => signal.test_rerun = rerun,
                    Err(err) => push_warning(
                        round.warnings,
                        format!("flaky test rerun skipped for {}: {err}", task.id),
                    ),
                }
            }
            Ok(Resolution::Failed(Box::new(signal)))
        }
        Verdict::Unverified(receipt) => {
            emit_gate(&receipt)?;
            if is_tree_changed_unverified(&receipt) {
                return tree_changed_failure(deps, &task.id, &job.worktree.path, receipt);
            }
            if is_transient_oracle(&receipt)
                && let Some(retry) = retry_transient(round, &task.id, &receipt.reasons.join("; "))
            {
                return Ok(retry);
            }
            blocked(format!(
                "the gate could not verify: {}",
                receipt.reasons.join("; ")
            ))
        }
        Verdict::Error(error) => {
            if !is_config_error(&error)
                && let Some(retry) = retry_transient(round, &task.id, &error)
            {
                return Ok(retry);
            }
            blocked(format!("the gate could not run: {error}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::receipt_duration_ms;
    use aaf_domain::gate::{
        CommandResult, GateMode, GateStatus, HeadLockReport, LockStatus, RECEIPT_SCHEMA_VERSION,
        Receipt, StoreComplianceReport,
    };

    fn receipt_with_durations(durations: &[u64]) -> Receipt {
        Receipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            repo_root: "/repo".to_string(),
            tree_fp: "fp".to_string(),
            head_sha: None,
            base_sha: None,
            mode: GateMode::Scoped,
            status: GateStatus::Clear,
            reasons: Vec::new(),
            config_hash: "cfg".to_string(),
            commands: durations
                .iter()
                .enumerate()
                .map(|(i, &duration_ms)| CommandResult {
                    name: format!("c{i}"),
                    command: "true".to_string(),
                    exit_code: Some(0),
                    timed_out: false,
                    duration_ms,
                    log_tail: String::new(),
                })
                .collect(),
            static_findings: Vec::new(),
            suppressions: 0,
            head_lock: HeadLockReport {
                status: LockStatus::Clear,
                lock_fp: String::new(),
                locked_changes: Vec::new(),
                approval: None,
            },
            store_compliance: StoreComplianceReport::not_applicable(),
            created_at: 0,
        }
    }

    #[test]
    fn receipt_duration_ms_sums_and_saturates() {
        assert_eq!(receipt_duration_ms(&receipt_with_durations(&[])), 0);
        assert_eq!(
            receipt_duration_ms(&receipt_with_durations(&[450, 750, 600])),
            1800
        );
        assert_eq!(
            receipt_duration_ms(&receipt_with_durations(&[u64::MAX, 10])),
            u64::MAX
        );
    }
}
