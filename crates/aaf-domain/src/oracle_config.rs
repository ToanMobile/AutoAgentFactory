//! Opt-in red->green & vacuity oracle mode for `[gate]`.

use serde::{Deserialize, Serialize};

/// Ordered so that higher tiers may only tighten it: `Off < Warn < Block`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum OracleMode {
    #[default]
    Off,
    Warn,
    Block,
}

impl OracleMode {
    pub fn is_off(&self) -> bool {
        *self == Self::Off
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Warn => "warn",
            Self::Block => "block",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "warn" => Some(Self::Warn),
            "block" => Some(Self::Block),
            _ => None,
        }
    }
}

/// How `OracleMode::Block` classifies `OracleOutcome::CompileRed`.
/// Ordered so higher tiers may only tighten: `Warn < Blocked < Unverified`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum OracleCompileRedPolicy {
    /// Downgrade `CompileRed` to a warning (tagged `behavioral_red_unproven`
    /// so post-gate cross-audit still inspects the new assertions).
    Warn,
    /// Map `CompileRed` to retryable `GateStatus::Blocked` (allowing T3 rework
    /// instead of 0-retry `Unverified`), while still downgrading all-`Added`
    /// new-module diffs to `Warn` + mandatory post-gate cross-audit.
    #[default]
    Blocked,
    /// Legacy fail-closed behavior: map `CompileRed` to `GateStatus::Unverified`.
    Unverified,
}

impl OracleCompileRedPolicy {
    pub fn is_default(&self) -> bool {
        *self == Self::Blocked
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Warn => "warn",
            Self::Blocked => "blocked",
            Self::Unverified => "unverified",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "warn" => Some(Self::Warn),
            "blocked" => Some(Self::Blocked),
            "unverified" => Some(Self::Unverified),
            _ => None,
        }
    }
}
