//! Pure verdict reduction helpers for the verification gate.

use std::time::Duration;

use aaf_domain::gate::{
    CommandResult, GateMode, GateStatus, HeadLockReport, LockStatus, StoreComplianceReport,
    StoreStatus,
};

use super::REASON_TREE_CHANGED;

/// Wall-clock and optional unpaused output-silence deadlines for one gate command.
#[derive(Debug, Clone, Copy)]
pub struct CommandTimeouts {
    pub wall: Duration,
    pub idle: Option<Duration>,
}

impl CommandTimeouts {
    pub fn from_secs(wall_secs: u64, idle_secs: u64) -> Self {
        Self {
            wall: Duration::from_secs(wall_secs),
            idle: (idle_secs > 0).then(|| Duration::from_secs(idle_secs)),
        }
    }
}

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

/// Written into a receipt that `run_commands_despite_head_lock` forced to
/// `Blocked` when everything else (commands, proof, oracle) passed: the head
/// lock is then the only block. Nothing else writes this text.
pub(crate) const HEAD_LOCK_ONLY_NOTE: &str =
    "note: head lock is the only block; commands, proof and oracle passed";

/// The reason text of a head lock that blocks, shared by the verdict and by
/// callers that re-add it after masking the lock.
pub(crate) fn head_lock_block_reason(report: &HeadLockReport) -> String {
    format!(
        "head lock: {} locked file(s) changed without a signed approval",
        report.locked_changes.len()
    )
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
        reasons.push(head_lock_block_reason(evidence.head_lock));
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use aaf_domain::gate::{ChangeKind, GateMode};

    use super::super::changes::Change;
    use super::super::runner::DiffEnvFiles;

    #[test]
    fn diff_env_files_unique_nonce_and_cleanup() {
        let changes = vec![
            Change {
                path: "src/a.rs".into(),
                kind: ChangeKind::Modified,
                old_path: None,
            },
            Change {
                path: "src/old.rs".into(),
                kind: ChangeKind::Deleted,
                old_path: None,
            },
        ];
        let first = DiffEnvFiles::write(GateMode::Scoped, Some("abc"), &changes).unwrap();
        let second = DiffEnvFiles::write(GateMode::Scoped, Some("abc"), &changes).unwrap();
        assert_ne!(first.changed_path, second.changed_path);
        assert_ne!(first.deleted_path, second.deleted_path);
        let (p1, p2) = (first.changed_path.clone(), first.deleted_path.clone());
        assert!(p1.exists() && p2.exists());
        drop(first);
        assert!(!p1.exists() && !p2.exists());
    }
}
