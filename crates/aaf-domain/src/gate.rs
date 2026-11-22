//! Verification gate results and the receipt that records them.

use serde::{Deserialize, Serialize};

pub const RECEIPT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warn,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateMode {
    Scoped,
    Full,
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum GateStatus {
    Clear,
    Blocked,
    Unverified,
}

impl GateStatus {
    /// CLI exit code: 0 for `Clear`, 2 for anything that is not verified.
    pub fn exit_code(self) -> i32 {
        match self {
            GateStatus::Clear => 0,
            GateStatus::Blocked | GateStatus::Unverified => 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaticFinding {
    /// Stable rule id, e.g. `SEC-001`.
    pub rule: String,
    pub severity: Severity,
    pub file: String,
    pub line: Option<u32>,
    pub message: String,
    pub fix: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedChange {
    pub path: String,
    pub change: ChangeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum LockStatus {
    /// No locked file was modified, deleted or renamed.
    Clear,
    /// Locked files changed and a valid signed approval covers `lock_fp`.
    Approved,
    Blocked,
}

/// A reviewer's sign-off on one exact set of locked-file changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    pub lock_fp: String,
    pub reviewer: String,
    /// `PASS` is the only verdict that unlocks.
    pub verdict: String,
    /// True when the reviewer ran on the same account as the author.
    pub same_account: bool,
    pub created_at: i64,
}

impl Approval {
    pub fn passes(&self, lock_fp: &str) -> bool {
        self.verdict == "PASS" && self.lock_fp == lock_fp
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadLockReport {
    pub status: LockStatus,
    /// Fingerprint of the locked changes; empty when `status` is `Clear`.
    pub lock_fp: String,
    pub locked_changes: Vec<LockedChange>,
    pub approval: Option<Approval>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreFinding {
    /// Stable rule id, e.g. `AND-003`.
    pub rule: String,
    pub severity: Severity,
    /// `android`, `ios` or `expo`.
    pub platform: String,
    /// The store guideline this maps to, in plain words.
    pub guideline: String,
    pub file: String,
    pub line: Option<u32>,
    pub message: String,
    pub fix: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StoreStatus {
    Clear,
    Blocked,
    /// Turned off by config; never counts as `Clear`.
    Skipped,
    /// No mobile project detected.
    NotApplicable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreComplianceReport {
    pub status: StoreStatus,
    pub critical: u32,
    pub warnings: u32,
    /// Values the scanner could not resolve statically (never silently ignored).
    pub unresolved: Vec<String>,
    pub findings: Vec<StoreFinding>,
}

impl StoreComplianceReport {
    pub fn not_applicable() -> StoreComplianceReport {
        StoreComplianceReport::empty(StoreStatus::NotApplicable)
    }

    pub fn skipped() -> StoreComplianceReport {
        StoreComplianceReport::empty(StoreStatus::Skipped)
    }

    fn empty(status: StoreStatus) -> StoreComplianceReport {
        StoreComplianceReport {
            status,
            critical: 0,
            warnings: 0,
            unresolved: Vec::new(),
            findings: Vec::new(),
        }
    }

    /// Build a report from findings: any `Critical` blocks.
    pub fn from_findings(findings: Vec<StoreFinding>, unresolved: Vec<String>) -> Self {
        let count = |sev: Severity| findings.iter().filter(|f| f.severity == sev).count();
        let critical = u32::try_from(count(Severity::Critical)).unwrap_or(u32::MAX);
        let warnings = u32::try_from(count(Severity::Warn)).unwrap_or(u32::MAX);
        StoreComplianceReport {
            status: if critical > 0 {
                StoreStatus::Blocked
            } else {
                StoreStatus::Clear
            },
            critical,
            warnings,
            unresolved,
            findings,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandResult {
    /// `lint`, `build`, `test_scoped` or `test_full`.
    pub name: String,
    pub command: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u64,
    /// Last bytes of combined output, for the human reading the receipt.
    pub log_tail: String,
}

impl CommandResult {
    pub fn passed(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub schema_version: u32,
    pub repo_root: String,
    pub tree_fp: String,
    pub head_sha: Option<String>,
    pub base_sha: Option<String>,
    pub mode: GateMode,
    pub status: GateStatus,
    /// Short reasons for a non-`Clear` status.
    pub reasons: Vec<String>,
    pub config_hash: String,
    pub commands: Vec<CommandResult>,
    pub static_findings: Vec<StaticFinding>,
    /// Number of inline `aaf:allow(...)` suppressions honoured.
    pub suppressions: u32,
    pub head_lock: HeadLockReport,
    pub store_compliance: StoreComplianceReport,
    pub created_at: i64,
}

/// One scored criterion from the 7 Golden Standards (`G1`..`G7`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoldenScore {
    pub id: String,
    pub score: u8,
    #[serde(default)]
    pub deduction_reason: String,
}

/// Signed post-gate cross-audit verdict bound to `(repo_root, tree_fp, config_hash)`
/// under HMAC domain `"audit\0"` so it survives restarts without re-auditing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostGateAuditRecord {
    pub repo_root: String,
    pub tree_fp: String,
    pub config_hash: String,
    /// `"PASS"` or `"FAIL"`.
    pub verdict: String,
    pub reviewer: String,
    pub same_account: bool,
    pub mechanical_score: u8,
    #[serde(default)]
    pub llm_average: Option<u8>,
    pub effective_score: u8,
    #[serde(default)]
    pub scores: Vec<GoldenScore>,
    pub created_at: i64,
}
