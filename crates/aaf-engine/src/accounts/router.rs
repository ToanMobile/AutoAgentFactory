//! Picks the next account to run on and tracks provider limits.

use std::path::{Path, PathBuf};

use aaf_domain::account::{AccountProfile, AccountStatus, ProviderKind};
use aaf_domain::ports::{AccountStore, Clock, LimitHit, RunStore};
use serde::{Deserialize, Serialize};

use super::AccountsError;
use super::lease::Lease;

const FIVE_HOURS: i64 = 5 * 3600;
const SEVEN_DAYS: i64 = 7 * 24 * 3600;

/// One row of `aaf accounts list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountView {
    pub id: String,
    pub provider: ProviderKind,
    pub config_dir: String,
    /// Display name; `id` stays the stable handle.
    pub name: String,
    #[serde(flatten)]
    pub status: AccountStatus,
    pub tokens_5h: u64,
    pub tokens_7d: u64,
}

pub struct Router<'a> {
    accounts: &'a dyn AccountStore,
    runs: &'a dyn RunStore,
    clock: &'a dyn Clock,
    leases_dir: PathBuf,
    max_concurrent_per_account: u32,
}

impl<'a> Router<'a> {
    pub fn new(
        accounts: &'a dyn AccountStore,
        runs: &'a dyn RunStore,
        clock: &'a dyn Clock,
        leases_dir: impl Into<PathBuf>,
        max_concurrent_per_account: u32,
    ) -> Self {
        Router {
            accounts,
            runs,
            clock,
            leases_dir: leases_dir.into(),
            // 0 would silently stall the whole factory.
            max_concurrent_per_account: max_concurrent_per_account.max(1),
        }
    }

    /// Upserts discovered accounts; existing cooldowns are kept by the store.
    pub fn sync(&self, profiles: &[AccountProfile]) -> Result<(), AccountsError> {
        for profile in profiles {
            self.accounts.upsert_account(profile)?;
        }
        Ok(())
    }

    /// The least-used account (tokens in the last 5 hours, ties by id) that
    /// is out of cooldown and has a free lease slot, together with its lease.
    pub fn next(&self) -> Result<Option<(AccountProfile, Lease)>, AccountsError> {
        self.next_where(&|_| true)
    }

    /// [`Router::next`] among the accounts `allow` accepts. With a filter
    /// that names one account this is "that account or nothing": the caller
    /// waits instead of moving to another one.
    pub fn next_where(
        &self,
        allow: &dyn Fn(&AccountProfile) -> bool,
    ) -> Result<Option<(AccountProfile, Lease)>, AccountsError> {
        let now = self.clock.now();
        let mut candidates = Vec::new();
        for state in self.accounts.list_accounts()? {
            // The store keeps the row of an account whose config dir was
            // removed; a run started on it could only fail.
            if state.in_cooldown(now)
                || !Path::new(&state.profile.config_dir).is_dir()
                || !allow(&state.profile)
            {
                continue;
            }
            let used = self
                .runs
                .tokens_since(&state.profile.id, now - FIVE_HOURS)?;
            candidates.push((used, state.profile));
        }
        candidates.sort_by(|a, b| (a.0, &a.1.id).cmp(&(b.0, &b.1.id)));
        for (_, profile) in candidates {
            if let Some(lease) = self.acquire_slot(&profile.id)? {
                return Ok(Some((profile, lease)));
            }
        }
        Ok(None)
    }

    /// Like [`Router::next_where`], but reserves at least one free slot across
    /// all ready (non-cooldown) accounts of this router when total ready
    /// capacity is at least 2. Used by `Purpose::Work` when `Gating` tasks are
    /// waiting for same-provider `PostAudit` so coding runs never starve finish.
    pub fn next_where_keeping_one(
        &self,
        allow: &dyn Fn(&AccountProfile) -> bool,
    ) -> Result<Option<(AccountProfile, Lease)>, AccountsError> {
        let now = self.clock.now();
        let mut ready_ids = Vec::new();
        let mut candidates = Vec::new();
        for state in self.accounts.list_accounts()? {
            if state.in_cooldown(now) || !Path::new(&state.profile.config_dir).is_dir() {
                continue;
            }
            ready_ids.push(state.profile.id.clone());
            if allow(&state.profile) {
                let used = self
                    .runs
                    .tokens_since(&state.profile.id, now - FIVE_HOURS)?;
                candidates.push((used, state.profile));
            }
        }
        let total_ready_slots = ready_ids
            .len()
            .saturating_mul(self.max_concurrent_per_account as usize);
        if total_ready_slots < 2 {
            candidates.sort_by(|a, b| (a.0, &a.1.id).cmp(&(b.0, &b.1.id)));
            for (_, profile) in candidates {
                if let Some(lease) = self.acquire_slot(&profile.id)? {
                    return Ok(Some((profile, lease)));
                }
            }
            return Ok(None);
        }
        candidates.sort_by(|a, b| (a.0, &a.1.id).cmp(&(b.0, &b.1.id)));
        for (_, profile) in candidates {
            if let Some(lease) = self.acquire_slot(&profile.id)? {
                let has_spare = ready_ids
                    .iter()
                    .any(|id| matches!(self.acquire_slot(id), Ok(Some(_))));
                if has_spare {
                    return Ok(Some((profile, lease)));
                }
                drop(lease);
                return Ok(None);
            }
        }
        Ok(None)
    }

    pub fn report_limit(&self, account_id: &str, hit: &LimitHit) -> Result<(), AccountsError> {
        let now = self.clock.now();
        let until = match hit.reset_at {
            Some(ts) if ts > now => ts,
            Some(_) => now + 60,
            None => now + FIVE_HOURS,
        };
        self.accounts.set_cooldown(account_id, Some(until))?;
        Ok(())
    }

    /// Snapshot for the CLI, sorted by id. `Leased` means every slot is
    /// taken right now; the probe lease is released before returning.
    pub fn status(&self, now: i64) -> Result<Vec<AccountView>, AccountsError> {
        let mut views = Vec::new();
        for state in self.accounts.list_accounts()? {
            let status = match state.cooldown_until {
                Some(until) if state.in_cooldown(now) => AccountStatus::Cooldown { until },
                _ if self.acquire_slot(&state.profile.id)?.is_none() => AccountStatus::Leased,
                _ => AccountStatus::Available,
            };
            let id = state.profile.id;
            views.push(AccountView {
                tokens_5h: self.runs.tokens_since(&id, now - FIVE_HOURS)?,
                tokens_7d: self.runs.tokens_since(&id, now - SEVEN_DAYS)?,
                id,
                provider: state.profile.provider,
                config_dir: state.profile.config_dir,
                name: state.profile.name,
                status,
            });
        }
        views.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(views)
    }

    /// Slot 0 is `<id>.lock` (the path the plan names); extra slots for
    /// `max_concurrent_per_account > 1` are `<id>.slot<n>.lock`.
    fn acquire_slot(&self, account_id: &str) -> Result<Option<Lease>, AccountsError> {
        for slot in 0..self.max_concurrent_per_account {
            let name = match slot {
                0 => account_id.to_string(),
                n => format!("{account_id}.slot{n}"),
            };
            if let Some(lease) = Lease::try_acquire(&self.leases_dir, &name)? {
                return Ok(Some(lease));
            }
        }
        Ok(None)
    }
}
