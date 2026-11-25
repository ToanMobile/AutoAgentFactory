//! Landing paths for gate-clear tasks: `branch`, `pr`, `pr-merge`, and `auto`
//! (fast-forward merge queue with PR-merge fallback on branch protection).

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use aaf_domain::config::AuditScope;
use aaf_domain::event::EventKind;
use aaf_domain::gate::{GateStatus, Receipt};
use aaf_domain::ports::{ClosedFingerprint, PortResult};
use aaf_domain::task::{Task, TaskState};
use serde_json::json;

use super::batch::Group;
use super::finish::{
    Resolution, Round, conflict_resolution, gate_payload, is_head_lock_only_block,
    is_tree_changed_unverified, push_warning, tree_changed_failure,
};
use super::post_audit;
use super::worker::GateJob;
use super::{Deps, merge_retry, unblock};
use crate::git::pr_merge::PrMergeOutcome;
use crate::git::{MergeOutcome, PrOutcome};
use crate::unblocker::FailureSignal;

const MERGE_ATTEMPTS: u32 = 3;

fn batch_note(group: &Group) -> String {
    let mut note = String::from(
        "## Batch gate receipt\n\nThis branch was not gated on its own. The receipt below is the \
         batch receipt of one integration tree, built from the base and these branches:\n\n",
    );
    for branch in &group.branches {
        note.push_str(&format!("- `{branch}`\n"));
    }
    note.push_str(
        "\n`tree_fp` (Tree) and Head below are those of that integration tree, not of this branch.",
    );
    note
}

fn compose_task_pr_body(deps: &Deps, job: &GateJob, task: &Task, receipt: &Receipt) -> String {
    let mut events = deps
        .store
        .events_for_task(&task.id, 100)
        .unwrap_or_default();
    if let Ok(plan_events) = deps.store.events_for_task_by_kinds(
        &task.id,
        &[EventKind::PlanConverged, EventKind::PlanGenerated],
        10,
    ) {
        for ev in plan_events {
            let dup = events.iter().any(|e| {
                (e.id.is_some() && e.id == ev.id)
                    || (e.kind == ev.kind
                        && e.created_at == ev.created_at
                        && e.payload_json == ev.payload_json)
            });
            if !dup {
                events.push(ev);
            }
        }
    }
    let audit = post_audit::load_verified_audit_verdict(deps, &job.worktree.path, receipt);
    let proof = crate::proof::show_proof(
        &job.worktree.path,
        &deps.settings.data_dir,
        Some(&receipt.tree_fp),
    )
    .ok();
    let ca = &deps.settings.repo.cross_audit;
    let handover = crate::handover::build_handover_from_parts_with_dual(
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
        ca.dual_review,
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

fn pr_note(
    deps: &Deps,
    job: &GateJob,
    task: &Task,
    receipt: &Receipt,
    gated_in: Option<&Group>,
) -> Option<String> {
    let ca = &deps.settings.repo.cross_audit;
    let has_handover_policy =
        ca.score_gate || ca.dual_review || ca.audit_scope != AuditScope::HeadLock;
    match gated_in {
        Some(group) => Some(batch_note(group)),
        None if has_handover_policy => {
            let composed = compose_task_pr_body(deps, job, task, receipt);
            handover_note_from_composed(receipt, &composed)
        }
        None => None,
    }
}

/// `land = "branch"`: leave commits on the local `aaf/<id>` branch for review.
pub(super) fn land_branch(deps: &Deps, job: &GateJob, task: &Task) -> PortResult<Resolution> {
    let why = format!(
        "land=branch: branch {} and its receipt are ready for review",
        job.worktree.branch
    );
    deps.move_to(&task.id, TaskState::Review, None, &why)?;
    unblock::root_succeeded(deps, task)?;
    Ok(Resolution::Settled(TaskState::Review))
}

/// `land = "pr"`: open a PR and move the task to Review (or skip PR with a warning).
pub(super) fn open_pr(
    deps: &Deps,
    job: &GateJob,
    task: &Task,
    receipt: &Receipt,
    gated_in: Option<&Group>,
    warnings: &Mutex<Vec<String>>,
) -> PortResult<Resolution> {
    let note = pr_note(deps, job, task, receipt, gated_in);
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

/// `land = "pr-merge"` (and fallback for `land = "auto"` after `PushRejected`):
/// open/update a PR and merge it through `deps.git.merge_pr`.
/// Must only be called when no `MergeLock` is held by the caller.
pub(super) fn land_pr_merge(
    deps: &Deps,
    job: &GateJob,
    task: &Task,
    receipt: &Receipt,
    gated_in: Option<&Group>,
    warnings: &Mutex<Vec<String>>,
) -> PortResult<Resolution> {
    let note = pr_note(deps, job, task, receipt, gated_in);
    let opened = deps
        .git
        .open_pr(&job.worktree, &task.title, receipt, note.as_deref())
        .unwrap_or_else(|err| {
            let reason = unblock::one_line(&err.to_string(), 300);
            push_warning(warnings, format!("{}: no pull request: {reason}", task.id));
            PrOutcome::Skipped { reason }
        });
    let url = match opened {
        PrOutcome::Opened { url } => {
            deps.emit(EventKind::PrOpened, &task.id, json!({ "url": url }))?;
            url
        }
        PrOutcome::Skipped { reason } => {
            let why = format!(
                "no pull request ({reason}); branch {} and its receipt are ready for review",
                job.worktree.branch
            );
            deps.move_to(&task.id, TaskState::Review, None, &why)?;
            unblock::root_succeeded(deps, task)?;
            return Ok(Resolution::Settled(TaskState::Review));
        }
    };
    let head_sha = receipt.head_sha.as_deref().unwrap_or("");
    let warn_err = |res: PortResult<()>, label: &str| {
        let err = res.err().map(|e| e.to_string());
        if let Some(ref e) = err {
            push_warning(warnings, format!("{}: {label}: {e}", task.id));
        }
        err
    };
    match deps.git.merge_pr(&job.worktree, &url, head_sha) {
        Ok(PrMergeOutcome::Merged {
            pr_number,
            head_sha,
            base_sha,
            local_sync_note,
            ..
        }) => {
            let why = format!("merged pull request #{pr_number} ({url})");
            deps.move_to(&task.id, TaskState::Done, None, &why)?;
            let fetch_error = warn_err(deps.git.fetch_base(), "post-merge fetch");
            let leftover = warn_err(deps.git.cleanup(&task.id), "post-merge cleanup");
            deps.emit(
                EventKind::MergeResult,
                &task.id,
                json!({
                    "merged": true,
                    "kind": "pr_merge",
                    "pr_number": pr_number,
                    "head_sha": head_sha,
                    "base_sha": base_sha,
                    "local_sync_note": local_sync_note,
                    "leftover": leftover,
                    "fetch_error": fetch_error,
                }),
            )?;
            close(deps, task)?;
            unblock::root_succeeded(deps, task)?;
            Ok(Resolution::Settled(TaskState::Done))
        }
        Ok(PrMergeOutcome::AlreadyMerged {
            pr_number,
            url: merged_url,
        }) => {
            let why = format!("pull request #{pr_number} already merged ({merged_url})");
            deps.move_to(&task.id, TaskState::Done, None, &why)?;
            let _ = warn_err(deps.git.fetch_base(), "post-merge fetch");
            let _ = warn_err(deps.git.cleanup(&task.id), "post-merge cleanup");
            close(deps, task)?;
            unblock::root_succeeded(deps, task)?;
            Ok(Resolution::Settled(TaskState::Done))
        }
        Ok(PrMergeOutcome::Blocked { code, reason, .. }) => {
            merge_retry::record_block(deps, &task.id, &url, head_sha, &code)?;
            let short = unblock::one_line(&reason, 240);
            push_warning(
                warnings,
                format!(
                    "{}: pull request {url} blocked from auto-merge ({code}): {short}",
                    task.id
                ),
            );
            let why = format!("pull request {url} (merge blocked: {code}: {short})");
            deps.move_to(&task.id, TaskState::Review, None, &why)?;
            unblock::root_succeeded(deps, task)?;
            Ok(Resolution::Settled(TaskState::Review))
        }
        Ok(PrMergeOutcome::DryRunClear { .. }) => {
            let why = format!("pull request {url} (dry-run clear)");
            deps.move_to(&task.id, TaskState::Review, None, &why)?;
            unblock::root_succeeded(deps, task)?;
            Ok(Resolution::Settled(TaskState::Review))
        }
        Err(err) => {
            merge_retry::record_block(deps, &task.id, &url, head_sha, "merge_error")?;
            let short = unblock::one_line(&err.to_string(), 240);
            push_warning(
                warnings,
                format!(
                    "{}: pull request {url} opened but auto-merge failed: {short}",
                    task.id
                ),
            );
            let why = format!("pull request {url} (auto-merge failed: {short})");
            deps.move_to(&task.id, TaskState::Review, None, &why)?;
            unblock::root_succeeded(deps, task)?;
            Ok(Resolution::Settled(TaskState::Review))
        }
    }
}

/// `land = "auto"`: direct fast-forward merge queue with PR-merge fallback when
/// remote branch protection rejects the push, and Review fallback when a local
/// base checkout is dirty.
pub(super) fn merge(
    deps: &Deps,
    job: &GateJob,
    task: &Task,
    initial_receipt: &Receipt,
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
                // Move to Done first so post-merge fetch/sync/cleanup failures cannot block a landed task.
                deps.move_to(&task.id, TaskState::Done, None, "merged into the base")?;
                let warn_err = |res: PortResult<()>, label: &str| {
                    let err = res.err().map(|e| e.to_string());
                    if let Some(ref e) = err {
                        push_warning(round.warnings, format!("{}: {label}: {e}", task.id));
                    }
                    err
                };
                let fetch_error = warn_err(deps.git.fetch_base(), "post-merge fetch");
                let local_sync_note = match deps.git.sync_local_base() {
                    Ok(note) => note,
                    Err(err) => {
                        push_warning(
                            round.warnings,
                            format!("{}: post-merge local base sync: {err}", task.id),
                        );
                        None
                    }
                };
                let leftover = warn_err(deps.git.cleanup(&task.id), "post-merge cleanup");
                deps.emit(
                    EventKind::MergeResult,
                    &task.id,
                    json!({
                        "merged": true,
                        "base_sha": new_base_sha,
                        "local_sync_note": local_sync_note,
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
            MergeOutcome::PushRejected { reason } => {
                // `merge_with_onto` has already dropped its `MergeLock`, so
                // opening a PR and calling `deps.git.merge_pr` cannot deadlock.
                let short = unblock::one_line(&reason, 240);
                push_warning(
                    round.warnings,
                    format!(
                        "{}: direct push rejected ({short}); falling back to PR merge",
                        task.id
                    ),
                );
                let latest = last_receipt.lock().ok().and_then(|mut slot| slot.take());
                let receipt = latest.as_ref().unwrap_or(initial_receipt);
                let gated_in = if latest.is_some() {
                    None
                } else {
                    batch_group.and_then(|id| round.groups.get(&id))
                };
                return land_pr_merge(deps, job, task, receipt, gated_in, round.warnings);
            }
            MergeOutcome::BaseCheckoutDirty { path } => {
                let warning = format!(
                    "warning: local base checkout {} has uncommitted changes; \
                     leaving branch {} in review instead of fast-forwarding",
                    path.display(),
                    job.worktree.branch
                );
                push_warning(round.warnings, format!("{}: {warning}", task.id));
                deps.store
                    .append_context_note(&task.id, &warning, deps.now())?;
                deps.move_to(&task.id, TaskState::Review, None, &warning)?;
                unblock::root_succeeded(deps, task)?;
                return Ok(Resolution::Settled(TaskState::Review));
            }
            MergeOutcome::Conflict { files } => {
                return conflict_resolution(deps, &task.id, files);
            }
            MergeOutcome::GateFailed => {
                let receipt = last_receipt.lock().ok().and_then(|mut slot| slot.take());
                if let Some(receipt) = receipt {
                    deps.emit(
                        EventKind::GateResult,
                        &task.id,
                        gate_payload(deps, &job.worktree.path, &receipt, None),
                    )?;
                    if !job.audited && is_head_lock_only_block(&receipt) {
                        return Ok(Resolution::NeedsAudit {
                            lock_fp: receipt.head_lock.lock_fp,
                        });
                    }
                    if is_tree_changed_unverified(&receipt) {
                        return tree_changed_failure(deps, &task.id, &job.worktree.path, receipt);
                    }
                    if receipt.status == GateStatus::Unverified {
                        let why =
                            format!("the gate could not verify: {}", receipt.reasons.join("; "));
                        unblock::block_untried(deps, &task.id, &why)?;
                        return Ok(Resolution::Settled(TaskState::Blocked));
                    }
                    let mut signal = FailureSignal::empty();
                    signal.gate = Some(receipt);
                    return Ok(Resolution::Failed(Box::new(signal)));
                }
                return Ok(Resolution::Failed(Box::new(FailureSignal::empty())));
            }
            MergeOutcome::NotFastForward => {
                skip_allowed.store(false, Ordering::Relaxed);
            }
        }
    }
    Ok(Resolution::Retry)
}

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
