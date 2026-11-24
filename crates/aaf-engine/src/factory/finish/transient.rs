//! Gate failures of the moment (a git timeout under host load, an oracle run
//! timed out or killed) that `settle` retries instead of blocking the task.

use aaf_domain::gate::Receipt;

use super::super::unblock;
use super::{Resolution, Round};

/// A gate failure of the moment is retried this often per task and `aaf run`
/// (within `MAX_GATE_RETRIES`); the next blocks with its own error.
const TRANSIENT_GATE_RETRIES: usize = 3;
const TRANSIENT_NOTE: &str = "gate retried after a transient failure for ";

/// `Retry` while retries are left; the run's warnings record and count them.
pub(super) fn retry_transient(round: &Round<'_>, task_id: &str, why: &str) -> Option<Resolution> {
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
pub(super) fn is_config_error(error: &str) -> bool {
    error
        .strip_prefix("io: ")
        .unwrap_or(error)
        .starts_with("config:")
}

/// The oracle runs only on a clear gate and adds its reason last: a timed
/// out or killed oracle run says nothing about the change.
pub(super) fn is_transient_oracle(receipt: &Receipt) -> bool {
    receipt.reasons.last().is_some_and(|reason| {
        matches!(
            reason.as_str(),
            "oracle: inconclusive (timed out)" | "oracle: inconclusive (killed by signal)"
        )
    })
}
