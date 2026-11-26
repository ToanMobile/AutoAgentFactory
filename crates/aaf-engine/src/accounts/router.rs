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
        // Serialize the acquire + spare-slot probe across concurrent processes
        // so two repos starting simultaneously cannot both claim the last two
        // slots while each observing the other's slot as still free.
        let _reserve_guard = Lease::acquire_blocking(&self.leases_dir, "_reserve")?;
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::{Arc, Barrier};

    use aaf_domain::account::{AccountProfile, ProviderKind};
    use aaf_domain::ports::{AccountStore, Clock};
    use tempfile::tempdir;

    use super::Router;
    use crate::db::Db;

    struct FixedClock(i64);
    impl Clock for FixedClock {
        fn now(&self) -> i64 {
            self.0
        }
    }

    #[test]
    fn next_where_keeping_one_is_atomic_under_concurrent_callers() {
        let dir = tempdir().unwrap();
        let db = Db::open(&dir.path().join("aaf.db")).unwrap();
        for id in ["claude-a", "claude-b"] {
            let cfg = dir.path().join(format!("cfg-{id}"));
            fs::create_dir_all(&cfg).unwrap();
            db.upsert_account(&AccountProfile {
                id: id.to_string(),
                provider: ProviderKind::ClaudeCode,
                config_dir: cfg.to_string_lossy().into_owned(),
                name: id.to_string(),
            })
            .unwrap();
        }
        let leases_dir = dir.path().join("leases");
        let db_path = dir.path().join("aaf.db");

        // Race two callers across 16 rounds: strictly one gets a lease and one
        // yields so the second slot stays free for Purpose::Audit.
        for _ in 0..16 {
            let barrier = Arc::new(Barrier::new(2));
            let (g1, g2) = std::thread::scope(|scope| {
                let spawn_one = |b: Arc<Barrier>| {
                    let db_path = db_path.clone();
                    let leases_dir = leases_dir.clone();
                    scope.spawn(move || {
                        let store = Db::open(&db_path).unwrap();
                        let clock = FixedClock(10_000);
                        let router = Router::new(&store, &store, &clock, leases_dir, 1);
                        b.wait();
                        router.next_where_keeping_one(&|_| true).unwrap()
                    })
                };
                let h1 = spawn_one(barrier.clone());
                let h2 = spawn_one(barrier);
                (h1.join().unwrap(), h2.join().unwrap())
            });
            let acquired = usize::from(g1.is_some()) + usize::from(g2.is_some());
            assert_eq!(
                acquired, 1,
                "concurrent next_where_keeping_one must never drain both slots"
            );
            // Verify the reserved slot is immediately available for Audit (`next()`).
            let clock = FixedClock(10_000);
            let router = Router::new(&db, &db, &clock, &leases_dir, 1);
            let audit = router
                .next()
                .unwrap()
                .expect("reserved audit slot must remain free");
            assert_ne!(
                g1.as_ref().or(g2.as_ref()).unwrap().0.id,
                audit.0.id,
                "audit must receive the spare slot"
            );
            drop((g1, g2, audit));
        }
    }
}
