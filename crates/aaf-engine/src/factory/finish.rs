//! What happens to a task once the gate has judged it: a pull request (the
//! default), the merge queue (`--auto-merge`), or the way back through the
//! unblocker. Runs on the gate thread, so merges of one repository are
//! naturally one at a time.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use aaf_domain::event::EventKind;
use aaf_domain::gate::{GateStatus, LockStatus, Receipt};
use aaf_domain::ports::{ClosedFingerprint, PortResult};
use aaf_domain::task::{Task, TaskState};
use serde_json::json;

use super::batch::{Group, Judged, Member, Verdict, gate_batch};
use super::worker::GateJob;
use super::{Deps, unblock};
use crate::git::{MergeOutcome, PrOutcome};
use crate::unblocker::FailureSignal;

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

    let proof_required = deps.settings.repo.proof.required;
    let mut batch: Vec<GateJob> = Vec::new();
    let mut alone: Vec<GateJob> = Vec::new();
    for job in jobs {
        match deps.gate.head_lock(&job.worktree.path) {
            Ok(lock) if lock.status == LockStatus::Clear => {
                // Proof artifacts are bound to each task worktree's tree_fp;
                // synthetic batch trees never carry a task's proof.
                if proof_required {
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

fn gate_payload(receipt: &Receipt, batch_group: Option<u32>) -> serde_json::Value {
    json!({
        "status": receipt.status,
        "reasons": receipt.reasons,
        "tree_fp": receipt.tree_fp,
        "gated_tree": receipt.repo_root,
        "batched": batch_group.is_some(),
        "batch_group": batch_group,
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
                gate_payload(&receipt, judged.batch_group),
            )?;
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
                gate_payload(&receipt, judged.batch_group),
            )?;
            let mut signal = FailureSignal::empty();
            signal.gate = Some(receipt);
            Ok(Resolution::Failed(Box::new(signal)))
        }
        Verdict::Unverified(receipt) => {
            deps.emit(
                EventKind::GateResult,
                &task.id,
                gate_payload(&receipt, judged.batch_group),
            )?;
            blocked(format!(
                "the gate could not verify: {}",
                receipt.reasons.join("; ")
            ))
        }
        Verdict::Error(error) => blocked(format!("the gate could not run: {error}")),
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
    let note = gated_in.map(batch_note);
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
    let regate = |tree: &Path| -> Result<bool, String> {
        if let Some(covered) = &covered
            && deps.git.base_sha().map_err(|e| e.to_string())? == *covered
        {
            return Ok(true);
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
        match deps.git.merge(&job.worktree, &regate)? {
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
                let mut signal = FailureSignal::empty();
                signal.gate = last_receipt.lock().ok().and_then(|mut slot| slot.take());
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
