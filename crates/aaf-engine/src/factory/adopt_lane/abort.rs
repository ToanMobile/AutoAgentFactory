//! Cancelling an adopted PR: the task goes `Cancelled`, the reason is recorded
//! and said on the PR. Nothing here touches `closed_fingerprints` or the
//! consecutive-failure counter: an adoption that fails is the PR's problem,
//! not the factory's.

use aaf_domain::event::EventKind;
use aaf_domain::task::TaskState;
use serde_json::json;

use super::lane::Lane;
use crate::factory::Deps;

const MAX_REASON_CHARS: usize = 1_500;

/// What the person who owns the PR does next, per cancel code.
fn advice(code: &str) -> &'static str {
    match code {
        "needs_human_head_lock" => {
            "This PR changes files only a person may approve. Review it and merge it yourself; \
             the factory will not."
        }
        "lock_changed" => {
            "The PR and the base both change a locked file, or the locked files moved since the \
             review. Rebase the PR onto the latest base, push, then adopt it again."
        }
        "needs_human_reviewers" => {
            "Adopting needs two reviewers of different providers. Install or configure a second \
             provider (`[[cross_audit.reviewers]]`), then adopt again."
        }
        "review_failed" => "Fix the findings above, push to the same PR, then adopt again.",
        "gate_blocked" => "Fix what blocks the gate, push to the same PR, then adopt again.",
        "head_changed" => {
            "The PR head (or the adopt worktree) is no longer the commit that was adopted. \
             Adopt the PR again to pick up the new head."
        }
        "merged_unpinned" => "The PR was merged at a different commit than the adopted one.",
        "merged_outside" => {
            "The PR was merged outside the factory, so nothing was verified here. Check what \
             landed."
        }
        "pr_not_open" => "The PR is no longer open.",
        "pr_is_draft" => "Mark the PR ready for review, then adopt again.",
        "ci_failed" => "Fix the failing checks, push, then adopt again.",
        "merge_conflict" => "Resolve the conflict with the base, push, then adopt again.",
        "retry_exhausted" => {
            "A temporary condition (CI, the merge lock, GitHub) did not clear in time. \
             Check it, then adopt again."
        }
        _ => "Look at the reason above, fix the PR, then adopt again.",
    }
}

/// The comment posted on the PR. It starts with the bot marker the repo's
/// comment guard requires.
pub(super) fn comment_body(pr_number: u64, code: &str, reason: &str) -> String {
    let mut reason: String = reason
        .chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .take(MAX_REASON_CHARS)
        .collect();
    if reason.trim().is_empty() {
        reason = "(no detail)".to_string();
    }
    format!(
        "🤖 aaf-bot: adopting PR #{pr_number} stopped: `{code}`.\n\n{reason}\n\n{}",
        advice(code)
    )
}

impl Lane<'_> {
    /// `Adopting -> Cancelled` plus the event and the PR comment. The state
    /// change comes first: when it does not apply (someone else already ended
    /// the task) nothing is recorded or posted. Returns a note for the run
    /// summary when something could not be recorded or posted.
    pub(super) fn cancel(&self, code: &str, reason: &str) -> Option<String> {
        if let Some(note) =
            record_cancel(self.deps, &self.task_id, code, reason, self.pin.pr_number)?
        {
            return Some(note);
        }
        let body = comment_body(self.pin.pr_number, code, reason);
        let posted = self
            .env
            .gh
            .comment_pr(&self.repo_root, self.pin.pr_number, &body);
        posted.err().map(|err| {
            format!(
                "{}: PR #{} comment not posted: {err}",
                self.task_id, self.pin.pr_number
            )
        })
    }
}

/// The state change and its events. `None`: the task was not `Adopting` any
/// more, nothing was done. `Some(None)`: cancelled. `Some(Some(note))`: not
/// (fully) recorded.
fn record_cancel(
    deps: &Deps,
    task_id: &str,
    code: &str,
    reason: &str,
    pr_number: u64,
) -> Option<Option<String>> {
    let now = deps.now();
    match deps.store.compare_and_set_state(
        task_id,
        TaskState::Adopting,
        TaskState::Cancelled,
        None,
        now,
    ) {
        Ok(true) => {}
        Ok(false) => return None,
        Err(err) => return Some(Some(format!("{task_id}: adopt not cancelled: {err}"))),
    }
    let recorded = deps
        .emit(
            EventKind::AdoptAborted,
            task_id,
            json!({ "code": code, "reason": reason, "pr_number": pr_number }),
        )
        .and_then(|()| {
            deps.emit(
                EventKind::TaskStateChanged,
                task_id,
                json!({ "to": TaskState::Cancelled.as_str(), "why": format!("adopt: {code}") }),
            )
        });
    Some(
        recorded
            .err()
            .map(|err| format!("{task_id}: adopt cancel not recorded: {err}")),
    )
}

/// Cancels a task whose pin cannot be read, so there is no PR to comment on.
pub(super) fn cancel_unpinned(
    deps: &Deps,
    task_id: &str,
    code: &str,
    reason: &str,
) -> Option<String> {
    record_cancel(deps, task_id, code, reason, 0).flatten()
}
