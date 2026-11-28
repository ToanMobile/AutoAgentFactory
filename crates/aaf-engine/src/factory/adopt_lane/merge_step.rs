//! The merge phase (I6): the locked merge gate on the pinned head, with a last
//! look right before `gh pr merge` that the task is still adopting and the
//! branch still at the pin.

use super::flow::{Done, Exit, Flow};
use super::lane::Lane;
use super::verify::blocked_exit;
use crate::pr_ops::{PrMergeOutcome, merge_pr_with_check};

impl Lane<'_> {
    pub(super) fn merge(&self) -> Flow<Done> {
        let req = self.request(false);
        let still_pinned = || self.still_pinned();
        let (outcome, _) = merge_pr_with_check(
            &req,
            None,
            &*self.env.gh,
            &*self.deps.clock,
            &|_| {},
            Some(&still_pinned),
        )
        .map_err(|e| Exit::retry("merge_error", e.to_string()))?;
        match outcome {
            // The merge phase's `AdoptStep` is already on record, so a merge that
            // landed during the call (or just before it) is this lane's own.
            PrMergeOutcome::Merged { .. } | PrMergeOutcome::AlreadyMerged { .. } => Ok(Done),
            PrMergeOutcome::Blocked {
                code,
                reason,
                receipt,
            } => Err(blocked_exit(&code, &reason, receipt.as_deref(), true)),
            PrMergeOutcome::DryRunClear { .. } | PrMergeOutcome::DryRunPendingHeadLock { .. } => {
                Err(Exit::abort(
                    "gate_blocked",
                    "the merge gate answered a dry run to a real merge",
                ))
            }
        }
    }
}
