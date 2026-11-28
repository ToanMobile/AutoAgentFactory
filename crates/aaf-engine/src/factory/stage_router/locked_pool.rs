//! The process-wide Antigravity lock around an [`AccountPool`] (R3-CONC-5).

use std::path::PathBuf;
use std::sync::Arc;

use aaf_domain::account::AccountProfile;
use aaf_domain::ports::{LimitHit, PortError, PortResult};

use crate::accounts::Lease;
use crate::factory::ports::{AccountPool, Grant, Purpose};
use crate::unblocker::QuotaRoute;

const ANTIGRAVITY_LEASE_STEM: &str = "provider-antigravity";

/// Wraps an [`AccountPool`] with an exclusive non-blocking file lock on
/// `<data_dir>/leases/provider-antigravity.lock` (R3-CONC-5) so at most one
/// Antigravity run executes at a time across processes (0 or 1+ accounts).
pub struct AntigravityLockedPool {
    inner: Arc<dyn AccountPool>,
    data_dir: PathBuf,
}

impl AntigravityLockedPool {
    pub fn new(inner: Arc<dyn AccountPool>, data_dir: PathBuf) -> Self {
        Self { inner, data_dir }
    }

    fn lock_then(
        &self,
        acquire: impl FnOnce(&dyn AccountPool) -> PortResult<Option<Grant>>,
    ) -> PortResult<Option<Grant>> {
        let leases_dir = self.data_dir.join("leases");
        let Some(lock) = Lease::try_acquire(&leases_dir, ANTIGRAVITY_LEASE_STEM)
            .map_err(|err| PortError::Io(err.to_string()))?
        else {
            return Ok(None);
        };
        Ok(acquire(&*self.inner)?.map(|g| g.with_extra_guard(Box::new(lock))))
    }
}

impl AccountPool for AntigravityLockedPool {
    fn acquire(&self) -> PortResult<Option<Grant>> {
        self.lock_then(|pool| pool.acquire())
    }

    fn acquire_for(&self, task_id: &str, purpose: Purpose) -> PortResult<Option<Grant>> {
        self.lock_then(|pool| pool.acquire_for(task_id, purpose))
    }

    fn after_limit(&self) -> PortResult<Option<QuotaRoute>> {
        self.inner.after_limit()
    }

    fn report_limit(&self, account_id: &str, hit: &LimitHit) -> PortResult<()> {
        self.inner.report_limit(account_id, hit)
    }

    fn profiles(&self) -> PortResult<Vec<AccountProfile>> {
        self.inner.profiles()
    }

    fn provider_account_count(&self) -> PortResult<usize> {
        self.inner.provider_account_count()
    }
}
