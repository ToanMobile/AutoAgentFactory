//! Tier-1 `[accounts]` configuration types: selection policy, aliases, and billing mode.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// How a job gets its account (PLAN C.8). Tier 1 only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountSelection {
    /// One account per provider for a repository. While that account is
    /// cooling down or leased the job waits; it never moves to another
    /// account of the same provider.
    Pinned,
    /// Default. A task keeps its account and a repository prefers its
    /// configured or last account, but a limit hit or a busy lease moves to
    /// another free account, which is then kept.
    #[default]
    Sticky,
    /// The free account with the fewest tokens in the last 5 hours.
    LeastUsed,
}

impl AccountSelection {
    pub fn as_str(self) -> &'static str {
        match self {
            AccountSelection::Pinned => "pinned",
            AccountSelection::Sticky => "sticky",
            AccountSelection::LeastUsed => "least_used",
        }
    }
}

/// Billing model for a configured account directory (`[accounts.billing]`). Tier 1 only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Billing {
    /// Flat-rate subscription (e.g. Claude Max/Pro): report tokens only, no USD estimate.
    #[default]
    Subscription,
    /// Pay-per-token API billing: estimate USD at default token rates.
    Api,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccountsConfig {
    pub discovery_globs: Vec<String>,
    pub max_concurrent_per_account: u32,
    pub selection: AccountSelection,
    /// Display names: config dir path (a leading `~` is the home dir) to the
    /// name shown for that account. Never changes an account id.
    pub aliases: BTreeMap<String, String>,
    /// Billing mode per config dir path (a leading `~` is the home dir).
    /// Accounts not listed default to [`Billing::Subscription`].
    pub billing: BTreeMap<String, Billing>,
}

impl Default for AccountsConfig {
    fn default() -> Self {
        AccountsConfig {
            discovery_globs: vec!["~/.claude".to_string(), "~/.claude-*".to_string()],
            max_concurrent_per_account: 1,
            selection: AccountSelection::default(),
            aliases: BTreeMap::new(),
            billing: BTreeMap::new(),
        }
    }
}
