//! Approximate USD estimation restricted to explicitly API-billed Claude accounts.

use std::collections::BTreeSet;
use std::path::Path;

use aaf_domain::account::AccountState;
use aaf_domain::config::AccountsConfig;
use serde::{Deserialize, Serialize};

use crate::accounts::discovery::{api_billed_account_ids, api_billed_dirs};

/// Per-million-token pricing used for approximate cost display (Anthropic defaults).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CostRates {
    pub input_per_million_usd: f64,
    pub output_per_million_usd: f64,
}

impl Default for CostRates {
    fn default() -> Self {
        Self {
            input_per_million_usd: 3.0,
            output_per_million_usd: 15.0,
        }
    }
}

impl CostRates {
    pub fn estimate_usd(self, tokens_in: u64, tokens_out: u64) -> f64 {
        let input = (tokens_in as f64 / 1_000_000.0) * self.input_per_million_usd;
        let output = (tokens_out as f64 / 1_000_000.0) * self.output_per_million_usd;
        ((input + output) * 1_000_000.0).round() / 1_000_000.0
    }
}

/// Wire indicator of whether `estimated_cost_usd` reflects API-billed accounts or is omitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostBasis {
    ApiAccounts,
    #[default]
    None,
}

impl CostBasis {
    pub fn from_estimate(estimated: Option<f64>) -> Self {
        if estimated.is_some() {
            Self::ApiAccounts
        } else {
            Self::None
        }
    }
}

/// Policy controlling which runs contribute to `estimated_cost_usd`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CostPolicy {
    pub rates: CostRates,
    pub api_accounts: BTreeSet<String>,
}

impl CostPolicy {
    /// Estimates USD strictly across runs belonging to `api_accounts`.
    ///
    /// Returns `None` when `api_accounts` is empty or no run in scope belongs
    /// to an API-billed account. Accumulates integer token counts first and
    /// calls [`CostRates::estimate_usd`] once to avoid per-run rounding drift.
    pub fn estimate<'a>(
        &self,
        per_account: impl IntoIterator<Item = (Option<&'a str>, u64, u64)>,
    ) -> Option<f64> {
        if self.api_accounts.is_empty() {
            return None;
        }
        let mut saw_api_run = false;
        let (mut tokens_in, mut tokens_out) = (0u64, 0u64);
        for (account_id, t_in, t_out) in per_account {
            if account_id.is_some_and(|id| self.api_accounts.contains(id)) {
                saw_api_run = true;
                tokens_in = tokens_in.saturating_add(t_in);
                tokens_out = tokens_out.saturating_add(t_out);
            }
        }
        saw_api_run.then(|| self.rates.estimate_usd(tokens_in, tokens_out))
    }
}

/// Pure helper that resolves API-billed Claude account ids into a [`CostPolicy`].
pub fn cost_policy_for(
    accounts_cfg: &AccountsConfig,
    home: &Path,
    accounts: &[AccountState],
    rates: CostRates,
) -> CostPolicy {
    let dirs = api_billed_dirs(accounts_cfg, home);
    let api_accounts = api_billed_account_ids(&dirs, accounts);
    CostPolicy {
        rates,
        api_accounts,
    }
}
