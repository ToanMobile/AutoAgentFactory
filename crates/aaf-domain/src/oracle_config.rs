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
