//! The verify phase: what GitHub says of the PR, the dry run of the merge gate
//! on the merged tree, and how its outcomes (and the codes of the real merge)
//! map to the lane's exits.

use aaf_domain::event::EventKind;
use aaf_domain::gate::Receipt;

use super::flow::{Exit, Flow};
use super::lane::Lane;
use super::lock_view::LockView;
use crate::git::PrInfo;
use crate::pr_ops::{PrMergeOutcome, merge_pr_with_check};

pub(super) struct Dry {
    pub base_sha: String,
    pub receipt: Receipt,
    /// The lock fingerprint of the merged tree (`""`: nothing locked).
    pub lock_fp: String,
}

/// Maps a `Blocked` code of the merge gate to the lane's exit. `merging` is
/// true for the real merge, where `cross_approval_required` means the locked
/// files moved after the signature.
pub(super) fn blocked_exit(
    code: &str,
    reason: &str,
    receipt: Option<&Receipt>,
    merging: bool,
) -> Exit {
    match code {
        "merge_conflict" => Exit::abort("merge_conflict", reason),
        "ci_pending" | "merge_in_progress" | "merge_error" => Exit::retry(code, reason),
        "ci_failed" => Exit::abort("ci_failed", reason),
        "head_moved" | "head_mismatch" => Exit::abort("head_changed", reason),
        "merged_unpinned" => Exit::abort("merged_unpinned", reason),
        "pr_not_open" => Exit::abort("pr_not_open", reason),
        "pr_is_draft" => Exit::abort("pr_is_draft", reason),
        // Someone cancelled the task: there is nothing left to do or say.
        "cancelled_before_merge" => Exit::Stop,
        "cross_approval_required" if merging => Exit::abort(
            "lock_changed",
            format!("the locked files changed after they were approved: {reason}"),
        ),
        _ => {
            let reasons = receipt
                .map(|r| r.reasons.join("; "))
                .filter(|r| !r.is_empty())
                .unwrap_or_else(|| reason.to_string());
            Exit::abort("gate_blocked", format!("{code}: {reasons}"))
        }
    }
}

impl Lane<'_> {
    /// The PR is merged at the pinned head. That is `Done` only when this task
    /// already reached the merge phase, which is entered after both reviewers
    /// passed and any signature succeeded (a turn that merged and then died, or
    /// a merge that landed during the call). Otherwise somebody merged it
    /// outside the factory: no gate, review or signature is on record, so the
    /// task is cancelled and no merge result is written.
    fn merged_at_pin(&self) -> Flow<()> {
        let steps = self.deps.store.events_for_task_by_kinds(
            &self.task_id,
            &[EventKind::AdoptStep],
            1_000,
        )?;
        let reached_merge = steps.iter().any(|event| {
            serde_json::from_str::<serde_json::Value>(&event.payload_json).is_ok_and(|v| {
                v["phase"].as_str() == Some("merge") && v["retry"].as_bool() != Some(true)
            })
        });
        if reached_merge {
            return Ok(());
        }
        Err(Exit::abort(
            "merged_outside",
            "the PR was merged outside the factory at the pinned head; no factory verification is \
             recorded",
        ))
    }

    /// What GitHub says of the PR right now (open, not a draft, at the pinned
    /// head, still on the adopted base). `None`: a turn that merged and then
    /// died left the PR merged at the pin already (see [`Self::merged_at_pin`]).
    pub(super) fn verify_pr(&self) -> Flow<Option<PrInfo>> {
        let pr = self
            .env
            .gh
            .view_pr(&self.repo_root, self.pin.pr_number)
            .map_err(|e| Exit::retry("gh_error", e.to_string()))?;
        if pr.state.eq_ignore_ascii_case("MERGED") {
            return if pr.head_ref_oid.eq_ignore_ascii_case(&self.pinned) {
                self.merged_at_pin().map(|()| None)
            } else {
                Err(Exit::abort(
                    "merged_unpinned",
                    format!(
                        "PR #{} is merged at {}, not the adopted {}",
                        pr.number, pr.head_ref_oid, self.pinned
                    ),
                ))
            };
        }
        if !pr.state.eq_ignore_ascii_case("OPEN") {
            return Err(Exit::abort(
                "pr_not_open",
                format!("PR #{} is {}", pr.number, pr.state),
            ));
        }
        if pr.is_draft {
            return Err(Exit::abort(
                "pr_is_draft",
                format!("PR #{} is a draft", pr.number),
            ));
        }
        if !pr.head_ref_oid.eq_ignore_ascii_case(&self.pinned) {
            return Err(Exit::abort(
                "head_changed",
                format!(
                    "PR #{} head is {}, not the adopted {}",
                    pr.number, pr.head_ref_oid, self.pinned
                ),
            ));
        }
        if pr.base_ref_name != self.pin.base_ref {
            return Err(Exit::abort(
                "gate_blocked",
                format!(
                    "PR #{} now targets {}, not the adopted base {}",
                    pr.number, pr.base_ref_name, self.pin.base_ref
                ),
            ));
        }
        Ok(Some(pr))
    }

    /// The merge gate as a dry run on the merged tree: real commands, no
    /// stored receipt, the head lock not a reason to skip them. `None`: the PR
    /// was merged at the pin in the meantime.
    pub(super) fn dry_run(&self) -> Flow<Option<Dry>> {
        let req = self.request(true);
        let (outcome, _) =
            merge_pr_with_check(&req, None, &*self.env.gh, &*self.deps.clock, &|_| {}, None)
                .map_err(|e| Exit::retry("merge_error", e.to_string()))?;
        match outcome {
            PrMergeOutcome::DryRunClear {
                base_sha, receipt, ..
            } => {
                let lock_fp = receipt.head_lock.lock_fp.clone();
                Ok(Some(Dry {
                    base_sha,
                    receipt: *receipt,
                    lock_fp,
                }))
            }
            PrMergeOutcome::DryRunPendingHeadLock {
                base_sha,
                receipt,
                lock_fp,
                ..
            } => Ok(Some(Dry {
                base_sha,
                receipt: *receipt,
                lock_fp,
            })),
            PrMergeOutcome::AlreadyMerged { .. } => self.merged_at_pin().map(|()| None),
            PrMergeOutcome::Blocked {
                code,
                reason,
                receipt,
            } => Err(blocked_exit(&code, &reason, receipt.as_deref(), false)),
            PrMergeOutcome::Merged { .. } => Err(Exit::abort(
                "gate_blocked",
                "the dry run of the merge gate reported a merge",
            )),
        }
    }

    /// Judges the lock by what the lane itself sees on the adopt worktree
    /// (`locked_changes`, never the receipt's status: a stale approval makes
    /// the dry run `Clear`) and compares it with the dry run's. Returns the
    /// fingerprint to sign, `None` when the PR locks nothing.
    pub(super) fn reconcile(&self, view: &LockView, dry: &Dry) -> Flow<Option<String>> {
        let locked = !view.report.locked_changes.is_empty();
        let mine = if locked {
            view.report.lock_fp.as_str()
        } else {
            ""
        };
        if mine != dry.lock_fp {
            return Err(Exit::abort(
                "lock_changed",
                "the PR and the base both change a locked file (or the base config locks more \
                 than it did); rebase the PR onto the latest base, push, and adopt it again",
            ));
        }
        Ok(locked.then(|| mine.to_string()))
    }
}
