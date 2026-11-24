//! [`AccountPool`] with the `accounts.selection` modes (PLAN C.8) on top of
//! the router's leases: which account a job of this repository may take.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use aaf_domain::account::{AccountProfile, AccountState, ProviderKind};
use aaf_domain::config::AccountSelection;
use aaf_domain::ports::{LimitHit, PortResult};
use serde::Serialize;

use super::adapters::RouterPool;
use super::ports::{AccountPool, Grant, Purpose};
use crate::accounts::selection::{self, Binding, BindingSource, RunHistory};
use crate::unblocker::QuotaRoute;

/// The pool of one `aaf run`: one repository, one provider.
pub struct SelectingPool {
    inner: RouterPool,
    selection: AccountSelection,
    repo_root: PathBuf,
    history: Arc<dyn RunHistory>,
    /// Pinned mode, repository without history: the account its first job
    /// got. The run that job records comes later, on another thread, and a
    /// second job started in between must not be given a second account.
    first_use: Mutex<Option<String>>,
}

/// The binding as `aaf accounts next` and `aaf doctor` show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BindingView {
    pub selection: AccountSelection,
    /// `pinned`: the account the repository is bound to. `sticky`: the one
    /// it prefers. `least_used`, or nothing to go by yet: `None`.
    pub account: Option<AccountProfile>,
    pub source: Option<BindingSource>,
    /// End of the bound account's cooldown, while it is cooling down.
    pub cooldown_until: Option<i64>,
    /// `account` of `.claude/settings.local.json`, as written there.
    pub settings_account: Option<String>,
    /// False when the settings file names no usable account of the
    /// executor's provider: the binding then falls back to the history.
    pub settings_account_known: bool,
}

impl SelectingPool {
    pub fn new(
        inner: RouterPool,
        selection: AccountSelection,
        repo_root: impl Into<PathBuf>,
        history: Arc<dyn RunHistory>,
    ) -> SelectingPool {
        SelectingPool {
            inner,
            selection,
            repo_root: repo_root.into(),
            history,
            first_use: Mutex::new(None),
        }
    }

    /// Accounts of the provider whose config dir still exists: a binding to
    /// a removed account would make the repository wait forever.
    fn usable(&self) -> PortResult<Vec<AccountState>> {
        let mut accounts = self.inner.provider_accounts()?;
        accounts.retain(|state| Path::new(&state.profile.config_dir).is_dir());
        Ok(accounts)
    }

    fn remembered(&self) -> Option<String> {
        // A poisoned lock only means a worker panicked; the value is whole.
        match self.first_use.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn remember(&self, account_id: &str) {
        let mut guard = match self.first_use.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.get_or_insert_with(|| account_id.to_string());
    }

    fn resolve(
        &self,
        provider: ProviderKind,
        accounts: &[AccountState],
    ) -> PortResult<Option<Binding>> {
        let profiles: Vec<AccountProfile> = accounts.iter().map(|s| s.profile.clone()).collect();
        let bound = selection::resolve(&self.repo_root, provider, &profiles, &*self.history)?;
        if bound.is_some() {
            return Ok(bound);
        }
        Ok(self
            .remembered()
            .and_then(|id| profiles.into_iter().find(|p| p.id == id))
            .map(|account| Binding {
                account,
                source: BindingSource::FirstUse,
            }))
    }

    fn pinned(&self, provider: ProviderKind) -> PortResult<Option<Grant>> {
        let Some(bound) = self.resolve(provider, &self.usable()?)? else {
            // No settings entry and no history: the least-used free account
            // becomes the binding.
            let grant = self.inner.acquire()?;
            if let Some(id) = grant.as_ref().and_then(Grant::account_id) {
                self.remember(id);
            }
            return Ok(grant);
        };
        // That account or nothing: busy or cooling down means wait.
        self.inner.acquire_where(&|p| p.id == bound.account.id)
    }

    fn acquire_for_audit(
        &self,
        provider: ProviderKind,
        task_id: &str,
    ) -> PortResult<Option<Grant>> {
        let of_task = self.history.last_account_of_task(task_id, provider)?;
        // The auditor should not be the author where there is a choice.
        let other = self
            .inner
            .acquire_where(&|p| Some(&p.id) != of_task.as_ref())?;
        match other {
            Some(grant) => Ok(Some(grant)),
            None => self.inner.acquire(),
        }
    }

    fn sticky(
        &self,
        provider: ProviderKind,
        task_id: &str,
        purpose: Purpose,
    ) -> PortResult<Option<Grant>> {
        if purpose == Purpose::Audit {
            return self.acquire_for_audit(provider, task_id);
        }
        let of_task = self.history.last_account_of_task(task_id, provider)?;
        let repo = self.repo_root.to_string_lossy();
        let of_repo = self.history.last_account_of_repo(&repo, provider)?;
        for preferred in [of_task, of_repo].into_iter().flatten() {
            if let Some(grant) = self.inner.acquire_where(&|p| p.id == preferred)? {
                return Ok(Some(grant));
            }
        }
        // Limited or busy: move on; the run recorded there makes it stick.
        self.inner.acquire()
    }

    /// What the repository is bound to right now, without leasing anything.
    pub fn binding(&self, now: i64) -> PortResult<BindingView> {
        let settings_account = selection::settings_account(&self.repo_root);
        let mut view = BindingView {
            selection: self.selection,
            account: None,
            source: None,
            cooldown_until: None,
            settings_account_known: false,
            settings_account,
        };
        let (Some(provider), false) = (
            self.inner.provider,
            self.selection == AccountSelection::LeastUsed,
        ) else {
            return Ok(view);
        };
        let accounts = self.usable()?;
        if let Some(bound) = self.resolve(provider, &accounts)? {
            view.settings_account_known = bound.source == BindingSource::Settings;
            view.cooldown_until = accounts
                .iter()
                .find(|s| s.profile.id == bound.account.id && s.in_cooldown(now))
                .and_then(|s| s.cooldown_until);
            view.source = Some(bound.source);
            view.account = Some(bound.account);
        }
        Ok(view)
    }
}

impl AccountPool for SelectingPool {
    fn acquire(&self) -> PortResult<Option<Grant>> {
        self.inner.acquire()
    }

    fn acquire_for(&self, task_id: &str, purpose: Purpose) -> PortResult<Option<Grant>> {
        // A custom executor has no provider and runs without an account.
        let Some(provider) = self.inner.provider else {
            return self.inner.acquire();
        };
        match self.selection {
            AccountSelection::LeastUsed if purpose == Purpose::Audit => {
                self.acquire_for_audit(provider, task_id)
            }
            AccountSelection::LeastUsed => self.inner.acquire(),
            AccountSelection::Pinned => self.pinned(provider),
            AccountSelection::Sticky => self.sticky(provider, task_id, purpose),
        }
    }

    fn after_limit(&self) -> PortResult<Option<QuotaRoute>> {
        if self.selection == AccountSelection::Pinned {
            return Ok(Some(QuotaRoute::Wait));
        }
        let now = self.inner.clock.now();
        let free = self.usable()?.iter().any(|state| !state.in_cooldown(now));
        Ok(Some(if free {
            QuotaRoute::Switch
        } else {
            QuotaRoute::Wait
        }))
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
