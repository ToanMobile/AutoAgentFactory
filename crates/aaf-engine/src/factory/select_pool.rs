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
    /// Optional Finish-First predicate: when `true` and `Purpose::Work` asks
    /// for a slot, keep at least one other ready slot free for `Purpose::Audit`.
    audit_reserve: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    /// When `true` (used for single-account read-only providers like Antigravity),
    /// `Purpose::Work` yields even the sole slot whenever `audit_reserve` fires.
    reserve_strict: bool,
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
            audit_reserve: None,
            reserve_strict: false,
        }
    }

    /// Enables Finish-First reservation: whenever `should_reserve()` returns
    /// `true`, `Purpose::Work` leaves at least one other ready slot unleased
    /// so same-provider `Purpose::Audit` jobs finish without starvation.
    pub fn with_audit_reserve(
        mut self,
        should_reserve: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Self {
        self.audit_reserve = Some(should_reserve);
        self.reserve_strict = false;
        self
    }

    /// Strict Finish-First reservation for single-account auxiliary providers
    /// (e.g. Antigravity shared between `plan` challenger and `Gating` audit):
    /// yields even the sole slot to `Purpose::Audit` while `should_reserve()` is true.
    pub fn with_strict_audit_reserve(
        mut self,
        should_reserve: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Self {
        self.audit_reserve = Some(should_reserve);
        self.reserve_strict = true;
        self
    }

    fn should_reserve_for_audit(&self, purpose: Purpose) -> bool {
        purpose == Purpose::Work && self.audit_reserve.as_ref().is_some_and(|pred| pred())
    }

    fn acquire_work_where(
        &self,
        reserve_one: bool,
        allow: &dyn Fn(&AccountProfile) -> bool,
    ) -> PortResult<Option<Grant>> {
        if reserve_one && self.reserve_strict {
            return Ok(None);
        }
        if reserve_one {
            self.inner.acquire_where_reserving_one(allow)
        } else {
            self.inner.acquire_where(allow)
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

    fn pinned(&self, provider: ProviderKind, reserve_one: bool) -> PortResult<Option<Grant>> {
        let Some(bound) = self.resolve(provider, &self.usable()?)? else {
            // No settings entry and no history: the least-used free account
            // becomes the binding.
            let grant = self.acquire_work_where(reserve_one, &|_| true)?;
            if let Some(id) = grant.as_ref().and_then(Grant::account_id) {
                self.remember(id);
            }
            return Ok(grant);
        };
        // That account or nothing: busy or cooling down means wait.
        self.acquire_work_where(reserve_one, &|p| p.id == bound.account.id)
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
        if other.is_some() {
            return Ok(other);
        }
        // V2-SEC-07: when multiple accounts exist in this provider's pool, wait
        // for a free non-author account instead of silently leasing the author's
        // own account (`same_account = true`), which wastes tokens on rejected
        // Tier-1 audits. Keep single-account fallback for solo-dev Tier-2 pools.
        if self.inner.provider_account_count()? > 1 {
            return Ok(None);
        }
        self.inner.acquire()
    }

    fn sticky(
        &self,
        provider: ProviderKind,
        task_id: &str,
        purpose: Purpose,
        reserve_one: bool,
    ) -> PortResult<Option<Grant>> {
        if purpose == Purpose::Audit {
            return self.acquire_for_audit(provider, task_id);
        }
        let of_task = self.history.last_account_of_task(task_id, provider)?;
        let settings_account_id = match selection::settings_account(&self.repo_root) {
            Some(wanted) => {
                let profiles: Vec<AccountProfile> = self
                    .usable()?
                    .into_iter()
                    .map(|state| state.profile)
                    .collect();
                selection::match_account(&wanted, &profiles).map(|profile| profile.id.clone())
            }
            None => None,
        };
        let repo = self.repo_root.to_string_lossy();
        let of_repo = self.history.last_account_of_repo(&repo, provider)?;
        for preferred in [of_task, settings_account_id, of_repo]
            .into_iter()
            .flatten()
        {
            if let Some(grant) = self.acquire_work_where(reserve_one, &|p| p.id == preferred)? {
                return Ok(Some(grant));
            }
        }
        // Limited or busy: move on; the run recorded there makes it stick.
        self.acquire_work_where(reserve_one, &|_| true)
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
        let reserve_one = self.should_reserve_for_audit(purpose);
        match self.selection {
            AccountSelection::LeastUsed if purpose == Purpose::Audit => {
                self.acquire_for_audit(provider, task_id)
            }
            AccountSelection::LeastUsed => self.acquire_work_where(reserve_one, &|_| true),
            AccountSelection::Pinned => self.pinned(provider, reserve_one),
            AccountSelection::Sticky => self.sticky(provider, task_id, purpose, reserve_one),
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};

    use aaf_domain::account::{AccountProfile, ProviderKind};
    use aaf_domain::config::AccountSelection;
    use aaf_domain::ports::{AccountStore, Clock, RunStore, TaskStore};
    use aaf_domain::run::{RunRecord, RunStatus};
    use aaf_domain::task::{ImmutableSpec, Priority, Task, TaskState};

    use super::{AccountPool, Purpose, RouterPool, SelectingPool};
    use crate::db::Db;

    struct TestClock(AtomicI64);

    impl Clock for TestClock {
        fn now(&self) -> i64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    #[test]
    fn sticky_honors_settings_local_account_and_fails_over_when_busy_or_limited() {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let db = Db::open(&root.join("aaf.db")).unwrap();
        for (id, name) in [("claude-home", "home"), ("claude-work", "work")] {
            let dir = root.join(format!("home/.claude-{name}"));
            fs::create_dir_all(&dir).unwrap();
            db.upsert_account(&AccountProfile {
                id: id.to_string(),
                provider: ProviderKind::ClaudeCode,
                config_dir: dir.to_string_lossy().into_owned(),
                name: name.to_string(),
            })
            .unwrap();
        }

        let repo = root.join("repo");
        fs::create_dir_all(repo.join(".claude")).unwrap();
        fs::write(
            repo.join(".claude/settings.local.json"),
            r#"{ "account": "work" }"#,
        )
        .unwrap();

        let store = Arc::new(db.clone());
        let clock = Arc::new(TestClock(AtomicI64::new(1_000)));
        let pool = SelectingPool::new(
            RouterPool {
                accounts: store.clone(),
                runs: store.clone(),
                clock,
                leases_dir: root.join("leases"),
                max_concurrent_per_account: 1,
                provider: Some(ProviderKind::ClaudeCode),
            },
            AccountSelection::Sticky,
            &repo,
            store,
        );

        // First run with no history honors `.claude/settings.local.json` ("work" -> "claude-work").
        let first = pool.acquire_for("t1", Purpose::Work).unwrap().unwrap();
        assert_eq!(first.account_id(), Some("claude-work"));

        // While "claude-work" is busy (leased), another work job fails over to "claude-home".
        let while_busy = pool.acquire_for("t2", Purpose::Work).unwrap().unwrap();
        assert_eq!(while_busy.account_id(), Some("claude-home"));
        drop(while_busy);
        drop(first);

        // When "claude-work" is cooling down (rate-limited), sticky still fails over to "claude-home".
        db.set_cooldown("claude-work", Some(9_000)).unwrap();
        let while_limited = pool.acquire_for("t3", Purpose::Work).unwrap().unwrap();
        assert_eq!(while_limited.account_id(), Some("claude-home"));
        drop(while_limited);

        // Once "claude-work" recovers:
        // - a task with its own history (`t3` on `claude-home`) keeps its task account (`of_task`),
        // - a fresh task (`t4`) prefers `.claude/settings.local.json` (`claude-work`) over repo history (`of_repo`).
        db.set_cooldown("claude-work", None).unwrap();
        let spec = ImmutableSpec {
            description: "test".to_string(),
            acceptance: vec!["ok".to_string()],
        };
        let repo_str = repo.to_string_lossy().into_owned();
        let mut t3 = Task::new_root("t3", &repo_str, "t3", spec, Priority::P2, 1);
        t3.state = TaskState::Ready;
        db.insert_task(&t3).unwrap();
        db.insert_run(&RunRecord {
            id: "run-t3-1".to_string(),
            task_id: "t3".to_string(),
            account_id: Some("claude-home".to_string()),
            executor: "claude".to_string(),
            worktree_path: String::new(),
            branch: String::new(),
            status: RunStatus::Succeeded,
            exit_code: Some(0),
            tokens_in: 0,
            tokens_out: 0,
            started_at: 1_000,
            ended_at: Some(1_001),
        })
        .unwrap();

        let t3_next = pool.acquire_for("t3", Purpose::Work).unwrap().unwrap();
        assert_eq!(t3_next.account_id(), Some("claude-home"));
        drop(t3_next);

        let t4_next = pool.acquire_for("t4", Purpose::Work).unwrap().unwrap();
        assert_eq!(t4_next.account_id(), Some("claude-work"));
        drop(t4_next);
    }

    #[test]
    fn audit_waits_for_non_author_when_multi_account_pool_and_falls_back_when_single() {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let db = Db::open(&root.join("aaf.db")).unwrap();
        let repo = root.join("repo");
        fs::create_dir_all(&repo).unwrap();
        let spec = ImmutableSpec {
            description: "audit test".to_string(),
            acceptance: vec!["ok".to_string()],
        };
        let repo_str = repo.to_string_lossy().into_owned();
        let mut t1 = Task::new_root("t1", &repo_str, "t1", spec, Priority::P2, 1);
        t1.state = TaskState::Gating;
        db.insert_task(&t1).unwrap();
        db.insert_run(&RunRecord {
            id: "run-t1".to_string(),
            task_id: "t1".to_string(),
            account_id: Some("claude-a".to_string()),
            executor: "claude".to_string(),
            worktree_path: String::new(),
            branch: String::new(),
            status: RunStatus::Succeeded,
            exit_code: Some(0),
            tokens_in: 0,
            tokens_out: 0,
            started_at: 1_000,
            ended_at: Some(1_001),
        })
        .unwrap();

        let dir_a = root.join("home/.claude-a");
        fs::create_dir_all(&dir_a).unwrap();
        db.upsert_account(&AccountProfile {
            id: "claude-a".to_string(),
            provider: ProviderKind::ClaudeCode,
            config_dir: dir_a.to_string_lossy().into_owned(),
            name: "a".to_string(),
        })
        .unwrap();

        let store = Arc::new(db.clone());
        let clock = Arc::new(TestClock(AtomicI64::new(1_000)));
        let make_pool = || {
            SelectingPool::new(
                RouterPool {
                    accounts: store.clone(),
                    runs: store.clone(),
                    clock: clock.clone(),
                    leases_dir: root.join("leases"),
                    max_concurrent_per_account: 1,
                    provider: Some(ProviderKind::ClaudeCode),
                },
                AccountSelection::Sticky,
                &repo,
                store.clone(),
            )
        };

        // Single-account pool keeps solo-dev fallback to author account.
        let single_pool = make_pool();
        let solo_grant = single_pool
            .acquire_for("t1", Purpose::Audit)
            .unwrap()
            .expect("single-account pool must fall back");
        assert_eq!(solo_grant.account_id(), Some("claude-a"));
        drop(solo_grant);

        // Multi-account pool: when non-author account is busy/cooling down,
        // Purpose::Audit returns None instead of leasing author's account.
        let dir_b = root.join("home/.claude-b");
        fs::create_dir_all(&dir_b).unwrap();
        db.upsert_account(&AccountProfile {
            id: "claude-b".to_string(),
            provider: ProviderKind::ClaudeCode,
            config_dir: dir_b.to_string_lossy().into_owned(),
            name: "b".to_string(),
        })
        .unwrap();
        db.set_cooldown("claude-b", Some(9_000)).unwrap();

        let multi_pool = make_pool();
        assert!(
            multi_pool
                .acquire_for("t1", Purpose::Audit)
                .unwrap()
                .is_none(),
            "multi-account pool must wait instead of leasing author account"
        );

        // Once claude-b recovers, audit acquires claude-b.
        db.set_cooldown("claude-b", None).unwrap();
        let ind_grant = multi_pool
            .acquire_for("t1", Purpose::Audit)
            .unwrap()
            .expect("independent account should be leased");
        assert_eq!(ind_grant.account_id(), Some("claude-b"));
    }

    #[test]
    fn audit_reserve_keeps_one_slot_for_post_audit_and_never_deadlocks_solo() {
        use std::sync::atomic::AtomicBool;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let repo = root.join("repo");
        fs::create_dir_all(&repo).unwrap();
        let db = Db::open(&root.join("aaf.db")).unwrap();
        for (id, name) in [("claude-a", "a"), ("claude-b", "b")] {
            let cfg = root.join(format!("home/.{id}"));
            fs::create_dir_all(&cfg).unwrap();
            db.upsert_account(&AccountProfile {
                id: id.into(),
                provider: ProviderKind::ClaudeCode,
                config_dir: cfg.to_string_lossy().into_owned(),
                name: name.into(),
            })
            .unwrap();
        }
        let store = Arc::new(db.clone());
        let clock = Arc::new(TestClock(AtomicI64::new(1_000)));
        let gating_pending = Arc::new(AtomicBool::new(true));
        let flag = gating_pending.clone();
        let pool = SelectingPool::new(
            RouterPool {
                accounts: store.clone(),
                runs: store.clone(),
                clock,
                leases_dir: root.join("leases"),
                max_concurrent_per_account: 1,
                provider: Some(ProviderKind::ClaudeCode),
            },
            AccountSelection::Sticky,
            &repo,
            store,
        )
        .with_audit_reserve(Arc::new(move || flag.load(Ordering::Relaxed)));

        // First coding work acquires one account because another ready slot stays free.
        let g1 = pool.acquire_for("t-work-1", Purpose::Work).unwrap();
        assert!(g1.is_some());
        // Second coding work defers so PostAudit keeps the last free slot.
        assert!(
            pool.acquire_for("t-work-2", Purpose::Work)
                .unwrap()
                .is_none()
        );
        // PostAudit immediately takes the reserved slot!
        let g_audit = pool.acquire_for("t-gate-1", Purpose::Audit).unwrap();
        assert!(g_audit.is_some());
        drop(g_audit);

        // Once gating finishes, coding work can use 100% of slots.
        gating_pending.store(false, Ordering::Relaxed);
        let g2 = pool.acquire_for("t-work-2", Purpose::Work).unwrap();
        assert!(g2.is_some());
        drop((g1, g2));

        // Solo ready account (other in cooldown): reserve must never deadlock Work.
        gating_pending.store(true, Ordering::Relaxed);
        db.set_cooldown("claude-b", Some(9_000)).unwrap();
        let solo = pool.acquire_for("t-work-3", Purpose::Work).unwrap();
        assert_eq!(
            solo.as_ref().and_then(super::Grant::account_id),
            Some("claude-a")
        );
    }
}
