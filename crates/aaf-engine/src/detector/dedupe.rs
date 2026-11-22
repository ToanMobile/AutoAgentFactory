//! Decides what a detected fingerprint becomes: a note on an open task,
//! nothing (recently closed), or a new task.

use aaf_domain::ports::{FingerprintStore, PortResult};

/// A fingerprint closed less than this long ago is not reopened as a new
/// task. Without the window a fix that did not hold would bounce between
/// "closed" and "new task" on every detection (ping-pong).
pub const SUPPRESS_WINDOW_SECS: i64 = 72 * 3600;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DedupeDecision {
    /// An open task already tracks this fingerprint.
    AttachToOpen {
        task_id: String,
    },
    /// Closed within the window (inclusive of exactly 72 hours ago): attach
    /// the occurrence to the old task instead of creating one.
    SuppressRecentlyClosed {
        task_id: String,
        closed_at: i64,
    },
    CreateTask,
}

/// `now` is Unix seconds from the caller's `Clock`. A store error is
/// returned, never read as "no duplicate": creating on error would flood the
/// board while the store is down.
pub fn decide(
    fingerprint: &str,
    repo_root: &str,
    store: &dyn FingerprintStore,
    now: i64,
) -> PortResult<DedupeDecision> {
    if let Some(task_id) = store.find_open_task(fingerprint, repo_root)? {
        return Ok(DedupeDecision::AttachToOpen { task_id });
    }
    let since = now.saturating_sub(SUPPRESS_WINDOW_SECS);
    match store.find_closed_since(fingerprint, repo_root, since)? {
        // Re-checked here so the window does not depend on every store
        // implementing `since` identically.
        Some(closed) if closed.closed_at >= since => Ok(DedupeDecision::SuppressRecentlyClosed {
            task_id: closed.task_id,
            closed_at: closed.closed_at,
        }),
        _ => Ok(DedupeDecision::CreateTask),
    }
}
