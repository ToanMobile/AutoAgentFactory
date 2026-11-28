//! Retry bookkeeping: the backoff, and the attempt count that survives a
//! restart because it is read back from the `AdoptStep` events.

use aaf_domain::event::{EventKind, FactoryEvent};
use aaf_domain::ports::PortError;
use serde_json::json;

use super::lane::Lane;

/// First wait; it doubles on every retry.
pub(super) const BASE_BACKOFF_SECS: i64 = 60;
pub(super) const MAX_BACKOFF_SECS: i64 = 30 * 60;
/// A PR still not through after this many retries is cancelled.
pub(super) const MAX_RETRIES: u32 = 12;

/// The wait before retry number `attempt` (1-based).
pub(crate) fn backoff_secs(attempt: u32) -> i64 {
    let doublings = attempt.saturating_sub(1).min(16);
    BASE_BACKOFF_SECS
        .saturating_mul(1_i64 << doublings)
        .min(MAX_BACKOFF_SECS)
}

fn is_retry(event: &FactoryEvent) -> bool {
    serde_json::from_str::<serde_json::Value>(&event.payload_json)
        .ok()
        .and_then(|v| v.get("retry").and_then(serde_json::Value::as_bool))
        == Some(true)
}

impl Lane<'_> {
    /// Retries scheduled so far for this task (every `AdoptStep` marked `retry`).
    pub(super) fn retries_so_far(&self) -> Result<u32, PortError> {
        let events = self.deps.store.events_for_task_by_kinds(
            &self.task_id,
            &[EventKind::AdoptStep],
            1_000,
        )?;
        Ok(u32::try_from(events.iter().filter(|e| is_retry(e)).count()).unwrap_or(u32::MAX))
    }

    /// Puts the task back for a later turn, or cancels it as `retry_exhausted`.
    /// Returns a note for the run summary when something could not be recorded.
    pub(super) fn schedule_retry(&self, code: &str, reason: &str) -> Option<String> {
        let attempt = match self.retries_so_far() {
            Ok(n) => n.saturating_add(1),
            Err(err) => return Some(format!("{}: adopt retry not recorded: {err}", self.task_id)),
        };
        if attempt > MAX_RETRIES {
            let why =
                format!("still not through after {MAX_RETRIES} retries; last: {code}: {reason}");
            return self.cancel("retry_exhausted", &why);
        }
        let wait = backoff_secs(attempt);
        let now = self.deps.now();
        let recorded = self
            .deps
            .store
            .set_retry_schedule(&self.task_id, Some(now + wait), 0, now)
            .and_then(|()| {
                self.deps.emit(
                    EventKind::AdoptStep,
                    &self.task_id,
                    json!({
                        "phase": self.phase.get(), "attempt": attempt, "retry": true,
                        "code": code, "reason": reason, "retry_in_secs": wait,
                    }),
                )
            });
        recorded
            .err()
            .map(|err| format!("{}: adopt retry not recorded: {err}", self.task_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_from_a_minute_to_half_an_hour() {
        let secs: Vec<i64> = (1..=8).map(backoff_secs).collect();
        assert_eq!(secs, [60, 120, 240, 480, 960, 1800, 1800, 1800]);
        assert_eq!(backoff_secs(0), 60);
        assert_eq!(backoff_secs(u32::MAX), 1800);
    }
}
