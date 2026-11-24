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

use super::batch::{Group, Judged, Member, Verdict, gate_batch};
use super::post_audit;
use super::worker::GateJob;
use super::{Deps, unblock};
use crate::cross_audit::compute_mechanical_score;
use crate::git::{MergeOutcome, PrOutcome};
use crate::unblocker::ladder;
use crate::unblocker::{FailureSignal, failure_digest, is_pure_test_failure};

/// Written by the gate thread before it reports a task, read by the
/// scheduler when the loop ends.
#[derive(Default)]
pub(super) struct GateStats {
    pub(super) runs: AtomicU32,
    pub(super) warnings: Mutex<Vec<String>>,
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
        let resolution = settle(deps, &job, judged, &mut round).map_err(|e| e.to_string());
        answer(job, resolution);
    }
    for (job, receipt) in cached_ready {
        let judged = Judged {
            task_id: job.task_id.clone(),
            verdict: Verdict::Clear(receipt),
            batch_group: None,
        };
        let resolution = settle(deps, &job, judged, &mut round).map_err(|e| e.to_string());
        answer(job, resolution);
    }
}

/// The base can move between the rebase and the fast-forward when another
/// process merges; each retry rebases again.
const MERGE_ATTEMPTS: u32 = 3;

pub(crate) enum Resolution {
    /// The task's state is final for this run (`Done`, `Review`, `Blocked`).
    Settled(TaskState),
    /// Locked files changed and nothing approves them yet.
    NeedsAudit {
        lock_fp: String,
    },
    /// Gate passed `Clear`, but `[cross_audit]` policy requires a post-gate audit.
    NeedsPostAudit {
        receipt: Box<Receipt>,
    },
    Failed(Box<FailureSignal>),
    /// Nothing failed; the task goes through the gate again.
    Retry,
}

/// State shared by the tasks of one gate round.
pub(crate) struct Round<'a> {
    /// The gated integration trees of this round. `Group::base` is the
    /// group's covered base: the base tip on which the next member of that
    /// group lands as a tree its own integration gate already judged. It
    /// starts as the tip the integration tree was built on, moves forward
    /// with every member of the same group that merges without a gate run
    /// of its own, and is gone for good once the chain breaks.
    ///
    /// The rule: a task merges without its own gate run only when what
    /// lands (the base at merge time, which then is the integration base
    /// plus members of its own group and nothing else, plus the task) is
    /// part of a tree that gated clear. Another group's merges, an outside
    /// commit, a member that failed to merge or a member that had to be
    /// gated again all leave the base somewhere no gate run of this group
    /// covers.
    pub groups: HashMap<u32, Group>,
    pub gate_runs: &'a AtomicU32,
    /// Things `aaf run` should tell its caller that blocked no task.
    pub warnings: &'a Mutex<Vec<String>>,
}

fn gate_payload(
    deps: &Deps,
    worktree: &Path,
    receipt: &Receipt,
    batch_group: Option<u32>,
) -> serde_json::Value {
    let static_warnings = receipt
        .static_findings
        .iter()
        .filter(|f| f.severity == Severity::Warn)
        .count();
    let static_critical = receipt
        .static_findings
        .iter()
        .filter(|f| f.severity == Severity::Critical)
        .count();
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
        "static_warnings": static_warnings,
        "static_critical": static_critical,
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
    round: &mut Round<'_>,
) -> PortResult<Resolution> {
    let task = deps.store.get_task(&job.task_id)?;
    let blocked = |why: String| -> PortResult<Resolution> {
        // Broken gate commands or unverified repo config must not count toward
        // the global consecutive-failure freeze across unrelated repos.
        unblock::block_untried(deps, &task.id, &why)?;
        Ok(Resolution::Settled(TaskState::Blocked))
    };
    match judged.verdict {
        Verdict::Clear(receipt) => {
            deps.emit(
                EventKind::GateResult,
                &task.id,
                gate_payload(deps, &job.worktree.path, &receipt, judged.batch_group),
            )?;
            if post_audit::needs_post_gate_audit(deps, job, &receipt) {
                return Ok(Resolution::NeedsPostAudit {
                    receipt: Box::new(receipt),
                });
            }
            if deps.settings.auto_merge {
                merge(deps, job, &task, judged.batch_group, round)
            } else {
                let gated_in = judged.batch_group.and_then(|id| round.groups.get(&id));
                open_pr(deps, job, &task, &receipt, gated_in, round.warnings)
            }
        }
        Verdict::Red(receipt) => {
            deps.emit(
                EventKind::GateResult,
                &task.id,
                gate_payload(deps, &job.worktree.path, &receipt, judged.batch_group),
            )?;
            let mut signal = FailureSignal::empty();
            signal.gate = Some(receipt);
            let digest = failure_digest(&signal);
            let rerun_eligible = judged.batch_group.is_none()
                && signal.gate.as_ref().is_some_and(is_pure_test_failure)
                && !ladder::tried(&task, UnblockTier::T2Mechanical, &digest);
            if rerun_eligible && let Some(receipt) = signal.gate.as_ref() {
                match deps.gate.rerun_failed_test(&job.worktree.path, receipt) {
                    Ok(rerun) => signal.test_rerun = rerun,
                    Err(err) => {
                        if let Ok(mut warnings) = round.warnings.lock() {
                            warnings
                                .push(format!("flaky test rerun skipped for {}: {err}", task.id));
                        }
                    }
                }
            }
            Ok(Resolution::Failed(Box::new(signal)))
        }
        Verdict::Unverified(receipt) => {
            deps.emit(
                EventKind::GateResult,
                &task.id,
                gate_payload(deps, &job.worktree.path, &receipt, judged.batch_group),
            )?;
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

/// A gate failure of the moment (a git timeout under host load, an oracle
/// run timed out or killed) is retried this often per task and `aaf run`
/// (within `MAX_GATE_RETRIES`); the next blocks with its own error.
const TRANSIENT_GATE_RETRIES: usize = 3;
const TRANSIENT_NOTE: &str = "gate retried after a transient failure for ";

/// `Retry` while retries are left; the run's warnings record and count them.
fn retry_transient(round: &Round<'_>, task_id: &str, why: &str) -> Option<Resolution> {
    let prefix = format!("{TRANSIENT_NOTE}{task_id}:");
    let mut warnings = round.warnings.lock().ok()?;
    let seen = warnings.iter().filter(|w| w.starts_with(&prefix)).count();
    if seen >= TRANSIENT_GATE_RETRIES {
        return None;
    }
    let why = unblock::one_line(why, 300);
    warnings.push(format!(
        "{prefix} {why} (retry {} of {TRANSIENT_GATE_RETRIES})",
        seen + 1
    ));
    Some(Resolution::Retry)
}

/// A broken repository config fails the same way every time.
fn is_config_error(error: &str) -> bool {
    error
        .strip_prefix("io: ")
        .unwrap_or(error)
        .starts_with("config:")
}

/// The oracle runs only on a clear gate and adds its reason last: a timed
/// out or killed oracle run says nothing about the change.
fn is_transient_oracle(receipt: &Receipt) -> bool {
    receipt.reasons.last().is_some_and(|reason| {
        matches!(
            reason.as_str(),
            "oracle: inconclusive (timed out)" | "oracle: inconclusive (killed by signal)"
        )
    })
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
    let without_receipt = if let Some(rest) = composed.strip_prefix(&base) {
        rest.trim()
    } else if let Some(head) = composed.strip_suffix(&base) {
        head.trim()
    } else if let Some((head, _)) = composed.split_once("\n## Gate receipt") {
        head.trim()
    } else {
        composed.trim()
    };
    let without_proof = without_receipt
        .split_once("\n## Verification Proof")
        .map_or(without_receipt, |(head, _)| head)
        .trim();
    (!without_proof.is_empty()).then(|| without_proof.to_string())
}

/// Default merge point: a pull request. Without `gh` or a remote the branch
/// and the signed receipt stay for a human, and the task still moves to
/// Review. So does a pull request that cannot be opened (a remote that is
/// not GitHub, `gh` not logged in, no network): the gate was clear and the
/// work is on its branch, which is not a failure of the task.
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
            if let Ok(mut warnings) = warnings.lock() {
                warnings.push(format!("{}: no pull request: {reason}", task.id));
            }
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
    round: &mut Round<'_>,
) -> PortResult<Resolution> {
    let last_receipt: Mutex<Option<Receipt>> = Mutex::new(None);
    // Taken, not copied: the group's chain is broken on every way out of
    // this function except the one that puts a new covered base back. A
    // task gated in its own worktree has no group: that gate ran on the
    // base the worktree was cut from, not on the current one.
    let covered = batch_group
        .and_then(|group| round.groups.get_mut(&group))
        .and_then(|group| group.base.take());
    let gated_here = AtomicBool::new(false);
    let gate_runs = round.gate_runs;
    let regate = |tree: &Path, onto_sha: &str| -> Result<bool, String> {
        if let Some(covered) = &covered {
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
                // Move to Done before fetching or deleting the worktree/branch so
                // a transient post-merge fetch failure or crash cannot block a
                // task that already landed on the remote.
                deps.move_to(&task.id, TaskState::Done, None, "merged into the base")?;
                // After a merge through the remote the local view of the base is
                // stale until fetched; a failed fetch does not undo the merge.
                let fetch_error = deps.git.fetch_base().err().map(|e| e.to_string());
                if let Some(ref err) = fetch_error
                    && let Ok(mut warnings) = round.warnings.lock()
                {
                    warnings.push(format!("{}: post-merge fetch: {err}", task.id));
                }
                // Done tasks must not pile up worktrees on disk. A failed
                // cleanup does not undo the merge; it is recorded with it.
                let leftover = deps.git.cleanup(&task.id).err().map(|e| e.to_string());
                if let Some(ref err) = leftover
                    && let Ok(mut warnings) = round.warnings.lock()
                {
                    warnings.push(format!("{}: post-merge cleanup: {err}", task.id));
                }
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
                // The batch verdict stays valid only along the chain of its
                // own group's merges. A merge that needed its own gate run
                // (the base had moved) leaves the chain broken: what follows
                // was never gated on this base.
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
                deps.emit(
                    EventKind::MergeResult,
                    &task.id,
                    json!({ "merged": false, "conflict": files }),
                )?;
                let mut signal = FailureSignal::empty();
                signal.merge_conflict_files = files;
                return Ok(Resolution::Failed(Box::new(signal)));
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
            MergeOutcome::NotFastForward => {}
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
