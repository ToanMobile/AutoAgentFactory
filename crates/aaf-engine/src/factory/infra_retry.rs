//! RFC-033: transient infra blocks come back on their own with backoff.

use aaf_domain::blocked::BlockedReason;
use aaf_domain::event::EventKind;
use aaf_domain::ports::PortResult;
use aaf_domain::task::{Task, TaskState};
use serde_json::json;

use super::Deps;
use super::unblock::block;

/// Automatic comebacks from an infra block (RFC-033); the store's
/// `wake_due_blocked_tasks` uses the same cap.
pub(crate) const INFRA_RETRY_MAX: u32 = 6;

/// Wait before infra comeback `attempt + 1`: 60 s doubling, capped at 30 min.
pub(crate) fn infra_backoff_secs(attempt: u32) -> i64 {
    (60_i64 << attempt.min(10)).min(1_800)
}

/// Blocks on upstream infra (RFC-033). While comebacks are left the task is
/// scheduled to return to Ready on its own: no webhook for these
/// intermediate blocks (the event carries `auto_retry_at` so `aaf notify`
/// stays quiet too), no `"unblocked:"` note, no freeze count. The last one
/// is a normal block that tells a person.
pub(crate) fn block_infra(
    deps: &Deps,
    task: &Task,
    reason: &BlockedReason,
    why: &str,
) -> PortResult<()> {
    let attempt = task.auto_recover_count;
    if attempt >= INFRA_RETRY_MAX {
        let why = format!("{why}; {INFRA_RETRY_MAX} automatic retries used");
        return block(deps, task, reason, "infra", &why, false);
    }
    let now = deps.now();
    let at = now + infra_backoff_secs(attempt);
    deps.store
        .set_state(&task.id, TaskState::Blocked, Some(reason), now)?;
    deps.store
        .set_retry_schedule(&task.id, Some(at), attempt, now)?;
    deps.emit(
        EventKind::TaskStateChanged,
        &task.id,
        json!({
            "to": TaskState::Blocked.as_str(),
            "why": why,
            "auto_retry_at": at,
            "auto_retry": attempt + 1,
        }),
    )
}
