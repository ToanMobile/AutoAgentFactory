//! Pure verdict reduction helpers for the verification gate.

use aaf_domain::gate::{
    CommandResult, GateMode, GateStatus, HeadLockReport, LockStatus, StoreComplianceReport,
    StoreStatus,
};

use super::REASON_TREE_CHANGED;

fn mode_name(mode: GateMode) -> &'static str {
    match mode {
        GateMode::Scoped => "scoped",
        GateMode::Full => "full",
        GateMode::Release => "release",
    }
}

/// What the stages found, reduced to what decides the status.
pub(crate) struct Evidence<'a> {
    /// The blocking reasons of [`super::hidden::inspect`]: files on disk that are
    /// not in the fingerprint and that the policy does not let pass.
    pub hidden: &'a [String],
    pub critical_findings: usize,
    pub head_lock: &'a HeadLockReport,
    pub store: &'a StoreComplianceReport,
    pub proof_block: Option<&'a str>,
    /// `None` when the commands were skipped because an earlier stage blocked.
    pub commands: Option<&'a [CommandResult]>,
    pub test_command_missing: bool,
    pub tree_changed: bool,
    pub mode: GateMode,
}

/// Blocking reasons found before any command runs.
fn early_blocks(evidence: &Evidence<'_>) -> Vec<String> {
    // First: with files nobody signed in the tree, nothing below describes
    // what a command would run on.
    let mut reasons: Vec<String> = evidence.hidden.to_vec();
    if evidence.critical_findings > 0 {
        reasons.push(format!(
            "static checks: {} critical finding(s)",
            evidence.critical_findings
        ));
    }
    if evidence.head_lock.status == LockStatus::Blocked {
        reasons.push(format!(
            "head lock: {} locked file(s) changed without a signed approval",
            evidence.head_lock.locked_changes.len()
        ));
    }
    if evidence.store.status == StoreStatus::Blocked {
        reasons.push(format!(
            "store compliance: {} critical finding(s)",
            evidence.store.critical
        ));
    }
    if let Some(reason) = evidence.proof_block {
        reasons.push(format!("proof required: {reason}"));
    }
    reasons
}

/// `Blocked` wins over `Unverified`; `Clear` needs no reason at all.
pub(crate) fn decide(evidence: &Evidence<'_>) -> (GateStatus, Vec<String>) {
    let mut reasons = early_blocks(evidence);
    match evidence.commands {
        None => reasons.push("commands skipped: an earlier stage already blocks".to_string()),
        Some(results) => {
            for result in results.iter().filter(|result| !result.passed()) {
                reasons.push(if result.timed_out {
                    format!("command {} timed out", result.name)
                } else if let Some(code) = result.exit_code {
                    format!("command {} failed (exit {code})", result.name)
                } else {
                    format!("command {} was killed by a signal", result.name)
                });
            }
        }
    }
    let blocked = !reasons.is_empty();
    let mut unverified = false;
    if evidence.test_command_missing {
        unverified = true;
        reasons.push(format!(
            "no test command is configured for mode {}",
            mode_name(evidence.mode)
        ));
    }
    if evidence.tree_changed {
        unverified = true;
        reasons.push(REASON_TREE_CHANGED.to_string());
    }
    let status = if blocked {
        GateStatus::Blocked
    } else if unverified {
        GateStatus::Unverified
    } else {
        GateStatus::Clear
    };
    (status, reasons)
}
