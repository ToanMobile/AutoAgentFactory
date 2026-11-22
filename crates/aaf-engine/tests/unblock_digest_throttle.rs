//! The failure digest of a blocked gate does not depend on what the machine
//! throttle did. The gate appends its notes (a wait for a heavy slot or for
//! memory pressure, a pause, a degraded run, a leftover build daemon) to the
//! receipt's reasons; whether one appears depends on the load and the other
//! gates running at that moment, not on the failure. Two identical failures,
//! one of which waited, must share one digest, or the ladder never sees a
//! repeat and requeues the task again instead of blocking it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "unblock_support.rs"]
mod support;

use aaf_domain::gate::GateStatus;
use aaf_engine::unblocker::{FailureSignal, failure_digest};
use support::receipt;

const FAILED: &str = "command test_scoped failed (exit 1)";

fn gate_failure(extra: &[&str]) -> FailureSignal {
    let mut reasons = vec![FAILED];
    reasons.extend_from_slice(extra);
    FailureSignal {
        gate: Some(receipt(GateStatus::Blocked, &reasons, "")),
        ..FailureSignal::empty()
    }
}

#[test]
fn a_throttle_note_does_not_change_the_digest_of_the_same_gate_failure() {
    let plain = failure_digest(&gate_failure(&[]));
    // The texts of `gate::runner::run_heavy`, one at a time and all at once.
    let notes = [
        "throttle: test_scoped waited 12s for a heavy slot or a calmer machine",
        "throttle: test_scoped ran degraded: CPU load stayed above 85% for 120s",
        "throttle: test_scoped was paused 3s under critical memory pressure",
        "throttle: gradle daemon still running pid=4242; builds are not using --no-daemon",
    ];
    for note in notes {
        assert_eq!(failure_digest(&gate_failure(&[note])), plain, "{note}");
    }
    assert_eq!(failure_digest(&gate_failure(&notes)), plain);
}

/// The neighbour: the reasons that describe the failure still count, so a
/// different failure behind the same throttle note is a different digest.
#[test]
fn the_reasons_that_describe_the_failure_still_separate_digests() {
    let note = "throttle: test_scoped waited 12s for a heavy slot or a calmer machine";
    let failed = failure_digest(&gate_failure(&[note]));
    let other = FailureSignal {
        gate: Some(receipt(
            GateStatus::Blocked,
            &["command test_scoped timed out", note],
            "",
        )),
        ..FailureSignal::empty()
    };
    assert_ne!(failure_digest(&other), failed);
    let hidden = failure_digest(&gate_failure(&[
        note,
        "hidden file: build/out.bin is ignored only outside a tracked .gitignore",
    ]));
    assert_ne!(hidden, failed);
}
