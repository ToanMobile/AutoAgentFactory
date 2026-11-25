//! Shared pure domain contracts and constants used across `aaf-engine` layers
//! without introducing upward module dependencies.

use crate::account::ProviderKind;
use crate::ports::PortResult;

/// Synthetic executor labels used for non-implementer runs (excluded from account stickiness).
pub const PLAN_EXECUTOR: &str = "plan";
pub const AUDIT_EXECUTOR: &str = "cross-audit";
pub const PROPOSER_EXECUTOR: &str = "unblock-proposer";

/// Maximum automatic retries for transient infrastructure failures before escalating.
pub const INFRA_RETRY_MAX: u32 = 6;

/// Relative path of the Tier-2 repository configuration file.
pub const REPO_CONFIG_REL: &str = ".autofactory/config.toml";

/// Prefix for every task branch created by AAF (`aaf/<task_id>`).
pub const BRANCH_PREFIX: &str = "aaf/";

/// Account history lookup implemented by SQLite storage and consumed by account routing.
pub trait RunHistory: Send + Sync {
    /// Account of the newest run of any task of `repo_root`, among accounts of `provider`.
    fn last_account_of_repo(
        &self,
        repo_root: &str,
        provider: ProviderKind,
    ) -> PortResult<Option<String>>;
    /// Account of the newest run of `task_id`, among accounts of `provider`.
    fn last_account_of_task(
        &self,
        task_id: &str,
        provider: ProviderKind,
    ) -> PortResult<Option<String>>;
}
