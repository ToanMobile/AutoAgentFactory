//! Landing strategy for gate-clear tasks (`[run].land`).
//!
//! Ordered so ambient (Tier-3/env) overlays may only lower it:
//! `Branch < Pr < PrMerge < Auto`.

use serde::{Deserialize, Serialize};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum LandMode {
    /// Leave commits on the local task branch (`aaf/<id>`) without pushing/PR.
    Branch = 0,
    /// Open/update a Pull Request and stop in `Review` (safe default).
    #[default]
    Pr = 1,
    /// Open a Pull Request and merge it through `aaf_engine::git::pr_merge::merge_pr`.
    PrMerge = 2,
    /// Attempt fast-forward push directly to base; fall back to `PrMerge` if
    /// rejected by remote branch protection.
    Auto = 3,
}

impl LandMode {
    pub fn is_default(&self) -> bool {
        *self == Self::Pr
    }

    pub fn auto_lands(self) -> bool {
        matches!(self, Self::PrMerge | Self::Auto)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Branch => "branch",
            Self::Pr => "pr",
            Self::PrMerge => "pr-merge",
            Self::Auto => "auto",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "branch" => Some(Self::Branch),
            "pr" | "pr-review" => Some(Self::Pr),
            "pr-merge" | "pr_merge" | "pr-auto-merge" | "pr_auto_merge" => Some(Self::PrMerge),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }
}
