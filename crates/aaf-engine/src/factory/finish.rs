//! What happens to a task once the gate has judged it: a pull request (the
//! default), the merge queue (`--auto-merge`), or the way back through the
//! unblocker. Runs on the gate thread, so merges of one repository are
//! naturally one at a time.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use aaf_domain::blocked::UnblockTier;
use aaf_domain::config::AuditScope;
use aaf_domain::event::EventKind;
use aaf_domain::gate::{GateStatus, LockStatus, Receipt, Severity};
use aaf_domain::ports::{ClosedFingerprint, PortResult};
use aaf_domain::task::{Task, TaskState};
use serde_json::json;

use super::batch::{Group, Judged, Member, Verdict, gate_batch, gate_one};
use super::post_audit;
use super::worker::GateJob;
use super::{Deps, unblock};
use crate::cross_audit::compute_mechanical_score;
use crate::git::{MergeOutcome, PrOutcome};
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

    // Settle cached-receipt jobs first (no gate needed; lands on base before
    // subsequent jobs rebase/integrate in `auto_merge`, avoids bubbles in PR mode).
    if !cached_ready.is_empty() {
        let mut round = Round::solo(stats);
        for (job, receipt) in cached_ready {
            let covered = (deps.settings.auto_merge && !deps.settings.repo.proof.required)
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

    // A batch of one has no integration tree; routing it at the front of `alone`
    // preserves board order while enabling pre-gate rebase and parallel PR gating.
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

    if !deps.settings.auto_merge && alone.len() > 1 {
        // PR mode: no shared base mutation; gate + settle in parallel and stream answers.
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
        // `auto_merge` (or single solo job): run end-to-end so job N+1 rebases onto job N.
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

fn push_warning(warnings: &Mutex<Vec<String>>, msg: String) {
    if let Ok(mut list) = warnings.lock() {
        list.push(msg);
    }
}

fn conflict_resolution(deps: &Deps, task_id: &str, files: Vec<String>) -> PortResult<Resolution> {
    deps.emit(
        EventKind::MergeResult,
        task_id,
        json!({ "merged": false, "conflict": files }),
    )?;
    let mut signal = FailureSignal::empty();
    signal.merge_conflict_files = files;
    Ok(Resolution::Failed(Box::new(signal)))
}

/// Rebases a solo job onto the current base before gating in `auto_merge` mode
/// (skipped when `proof.required`, which binds commit SHAs to the signed proof).
fn pre_gate_rebase(deps: &Deps, job: &GateJob) -> PreRebase {
    if !deps.settings.auto_merge || deps.settings.repo.proof.required {
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

const MERGE_ATTEMPTS: u32 = 3;

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

fn gate_payload(
    deps: &Deps,
    worktree: &Path,
    receipt: &Receipt,
    batch_group: Option<u32>,
) -> serde_json::Value {
    let sev_count = |s: Severity| {
        receipt
            .static_findings
            .iter()
            .filter(|f| f.severity == s)
            .count()
    };
    let commands_passed = receipt.commands.iter().filter(|c| c.passed()).count();
    let proof_ok = post_audit::proof_verified(deps, worktree, receipt);
    let mechanical_score = compute_mechanical_score(receipt, proof_ok, false, false).score;
    json!({
        "status": receipt.status,
        "reasons": receipt.reasons,
        "tree_fp": receipt.tree_fp,
        "config_hash": receipt.config_hash,
        "gated_tree": receipt.repo_root,
        "batched": batch_group.is_some(),
        "batch_group": batch_group,
        "static_warnings": sev_count(Severity::Warn),
        "static_critical": sev_count(Severity::Critical),
        "store_status": receipt.store_compliance.status,
        "store_warnings": receipt.store_compliance.warnings,
        "store_critical": receipt.store_compliance.critical,
        "store_unresolved": receipt.store_compliance.unresolved.len(),
        "head_lock_status": receipt.head_lock.status,
        "suppressions": receipt.suppressions,
        "commands_passed": commands_passed,
        "commands_total": receipt.commands.len(),
        "mechanical_score": mechanical_score,
    })
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
            emit_gate(&receipt)?;
            if post_audit::needs_post_gate_audit(deps, job, &receipt) {
                return Ok(Resolution::NeedsPostAudit {
                    receipt: Box::new(receipt),
                });
            }
            if deps.settings.auto_merge {
                merge(deps, job, &task, judged.batch_group, covered_base, round)
            } else {
                let gated_in = judged.batch_group.and_then(|id| round.groups.get(&id));
                open_pr(deps, job, &task, &receipt, gated_in, round.warnings)
            }
        }
        Verdict::Red(receipt) => {
            emit_gate(&receipt)?;
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

/// What a pull request says above its receipt summary when the gate ran on
/// an integration tree: the receipt is not this branch's own.
fn batch_note(group: &Group) -> String {
    let mut note = String::from(
        "## Batch gate receipt\n\n\
         This branch was not gated on its own. The receipt below is the batch \
         receipt of one integration tree, built from the base and these branches:\n\n",
    );
    for branch in &group.branches {
        note.push_str(&format!("- `{branch}`\n"));
    }
    note.push_str(
        "\n`tree_fp` (Tree) and Head below are those of that integration tree, \
         not of this branch.",
    );
    note
}

/// Composes a handover-enriched PR body for `task` from DB state plus verified
/// receipt, optional signed post-gate audit verdict, and optional signed proof.
pub(crate) fn compose_task_pr_body(
    deps: &Deps,
    job: &GateJob,
    task: &Task,
    receipt: &Receipt,
) -> String {
    let events = deps
        .store
        .events_for_task(&task.id, 100)
        .unwrap_or_default();
    let audit = post_audit::load_verified_audit_verdict(deps, &job.worktree.path, receipt);
    let proof = crate::proof::show_proof(
        &job.worktree.path,
        &deps.settings.data_dir,
        Some(&receipt.tree_fp),
    )
    .ok();
    let ca = &deps.settings.repo.cross_audit;
    let handover = crate::handover::build_handover_from_parts(
        task,
        &events,
        Some(receipt),
        audit.as_ref(),
        proof.as_ref(),
        deps.settings.repo.proof.required,
        ca.score_gate,
        ca.min_average_score,
        ca.min_criterion_score,
        ca.require_different_provider,
    );
    crate::handover::compose_pr_body(receipt, Some(&handover))
}

fn handover_note_from_composed(receipt: &Receipt, composed: &str) -> Option<String> {
    let base = crate::git::pr_body(receipt);
    let without_receipt = composed
        .strip_prefix(&base)
        .or_else(|| composed.strip_suffix(&base))
        .or_else(|| composed.split_once("\n## Gate receipt").map(|(h, _)| h))
        .unwrap_or(composed)
        .trim();
    let without_proof = without_receipt
        .split_once("\n## Verification Proof")
        .map_or(without_receipt, |(head, _)| head)
        .trim();
    (!without_proof.is_empty()).then(|| without_proof.to_string())
}

/// Default merge point: open a PR and move the task to Review (or skip PR with a warning).
fn open_pr(
    deps: &Deps,
    job: &GateJob,
    task: &Task,
    receipt: &Receipt,
    gated_in: Option<&Group>,
    warnings: &Mutex<Vec<String>>,
) -> PortResult<Resolution> {
    let ca = &deps.settings.repo.cross_audit;
    let has_handover_policy = ca.score_gate || ca.audit_scope != AuditScope::HeadLock;
    let note = match gated_in {
        Some(group) => Some(batch_note(group)),
        None if has_handover_policy => {
            let composed = compose_task_pr_body(deps, job, task, receipt);
            handover_note_from_composed(receipt, &composed)
        }
        None => None,
    };
    let opened = deps
        .git
        .open_pr(&job.worktree, &task.title, receipt, note.as_deref())
        .unwrap_or_else(|err| {
            let reason = unblock::one_line(&err.to_string(), 300);
            push_warning(warnings, format!("{}: no pull request: {reason}", task.id));
            PrOutcome::Skipped { reason }
        });
    let why = match opened {
        PrOutcome::Opened { url } => {
            deps.emit(EventKind::PrOpened, &task.id, json!({ "url": url }))?;
            format!("pull request {url}")
        }
        PrOutcome::Skipped { reason } => format!(
            "no pull request ({reason}); branch {} and its receipt are ready for review",
            job.worktree.branch
        ),
    };
    deps.move_to(&task.id, TaskState::Review, None, &why)?;
    unblock::root_succeeded(deps, task)?;
    Ok(Resolution::Settled(TaskState::Review))
}

fn merge(
    deps: &Deps,
    job: &GateJob,
    task: &Task,
    batch_group: Option<u32>,
    covered_base: Option<String>,
    round: &mut Round<'_>,
) -> PortResult<Resolution> {
    let last_receipt: Mutex<Option<Receipt>> = Mutex::new(None);
    // Taken, not copied: the group's chain is broken on every way out of
    // this function except the one that puts a new covered base back. A solo
    // job carries `covered_base` only when rebased onto a stable base before gating.
    let covered = batch_group
        .and_then(|group| round.groups.get_mut(&group))
        .and_then(|group| group.base.take())
        .or(covered_base);
    let skip_allowed = AtomicBool::new(covered.is_some());
    let gated_here = AtomicBool::new(false);
    let gate_runs = round.gate_runs;
    let regate = |tree: &Path, onto_sha: &str| -> Result<bool, String> {
        if skip_allowed.load(Ordering::Relaxed)
            && let Some(covered) = &covered
        {
            let matches_covered = if !onto_sha.is_empty() {
                onto_sha == *covered
            } else {
                deps.git.base_sha().map_err(|e| e.to_string())? == *covered
            };
            if matches_covered {
                return Ok(true);
            }
        }
        gated_here.store(true, Ordering::Relaxed);
        gate_runs.fetch_add(1, Ordering::Relaxed);
        let receipt = deps.gate.run(tree).map_err(|e| e.to_string())?;
        let clear = receipt.status == GateStatus::Clear;
        if let Ok(mut slot) = last_receipt.lock() {
            *slot = Some(receipt);
        }
        Ok(clear)
    };
    for _ in 0..MERGE_ATTEMPTS {
        match deps.git.merge_with_onto(&job.worktree, &regate)? {
            MergeOutcome::Merged { new_base_sha } => {
                // Move to Done first so post-merge fetch/cleanup failures cannot block a landed task.
                deps.move_to(&task.id, TaskState::Done, None, "merged into the base")?;
                let warn_err = |res: PortResult<()>, label: &str| {
                    let err = res.err().map(|e| e.to_string());
                    if let Some(ref e) = err {
                        push_warning(round.warnings, format!("{}: {label}: {e}", task.id));
                    }
                    err
                };
                let fetch_error = warn_err(deps.git.fetch_base(), "post-merge fetch");
                let leftover = warn_err(deps.git.cleanup(&task.id), "post-merge cleanup");
                deps.emit(
                    EventKind::MergeResult,
                    &task.id,
                    json!({
                        "merged": true,
                        "base_sha": new_base_sha,
                        "leftover": leftover,
                        "fetch_error": fetch_error,
                    }),
                )?;
                if !gated_here.load(Ordering::Relaxed)
                    && let Some(group) = batch_group.and_then(|g| round.groups.get_mut(&g))
                {
                    group.base = Some(new_base_sha);
                }
                close(deps, task)?;
                unblock::root_succeeded(deps, task)?;
                return Ok(Resolution::Settled(TaskState::Done));
            }
            MergeOutcome::Conflict { files } => {
                return conflict_resolution(deps, &task.id, files);
            }
            MergeOutcome::GateFailed => {
                let receipt = last_receipt.lock().ok().and_then(|mut slot| slot.take());
                if let Some(ref receipt) = receipt {
                    deps.emit(
                        EventKind::GateResult,
                        &task.id,
                        gate_payload(deps, &job.worktree.path, receipt, None),
                    )?;
                    if receipt.status == GateStatus::Unverified {
                        let why =
                            format!("the gate could not verify: {}", receipt.reasons.join("; "));
                        unblock::block_untried(deps, &task.id, &why)?;
                        return Ok(Resolution::Settled(TaskState::Blocked));
                    }
                }
                let mut signal = FailureSignal::empty();
                signal.gate = receipt;
                return Ok(Resolution::Failed(Box::new(signal)));
            }
            MergeOutcome::NotFastForward => {
                skip_allowed.store(false, Ordering::Relaxed);
            }
        }
    }
    Ok(Resolution::Retry)
}

/// Remembers the fingerprint of a finished task so the detector does not
/// file the same failure again right away.
pub(crate) fn close(deps: &Deps, task: &Task) -> PortResult<()> {
    let Some(fingerprint) = &task.fingerprint else {
        return Ok(());
    };
    deps.store.record_closed(&ClosedFingerprint {
        fingerprint: fingerprint.clone(),
        repo_root: task.repo_root.clone(),
        task_id: task.id.clone(),
        closed_at: deps.now(),
    })
}
