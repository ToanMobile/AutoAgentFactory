//! Production implementations of the factory's ports on the engine modules.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aaf_domain::account::{AccountProfile, AccountState, ProviderKind};
use aaf_domain::config::{MachineConfig, RepoConfig};
use aaf_domain::gate::{GateMode, GateStatus, HeadLockReport, RECEIPT_SCHEMA_VERSION, Receipt};
use aaf_domain::ports::{AccountStore, Clock, LimitHit, PortError, PortResult, RunStore};

use super::ports::{AccountPool, GateRunner, Grant, Notifier};
use crate::accounts::Router;
use crate::db::private;
use crate::gate::hidden::When;
use crate::gate::runner::{DiffEnvFiles, HeavyContext};
use crate::gate::{self, HiddenPolicy};
use crate::throttle::{Progress, SystemProbe, Throttle};
use crate::unblocker::{RerunResult, is_pure_test_failure};
use crate::webhook::{self, WebhookEvent, WebhookOptions};

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> i64 {
        // A clock before 1970 is a broken machine; 0 keeps time monotone
        // enough for the stores instead of panicking.
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
    }
}

/// The scoped gate of the repository's configuration, run on whichever
/// worktree the factory points it at.
pub struct RealGate {
    pub data_dir: PathBuf,
    /// Loaded from the main checkout, never from the worktree under test, so
    /// an agent cannot loosen its own gate.
    pub cfg: RepoConfig,
    pub clock: Arc<dyn Clock>,
    /// Machine load limits for heavy gate commands (PLAN C.11).
    pub machine: MachineConfig,
    /// Told what the throttle is doing (waiting for a slot, for memory, for
    /// the load; pausing) and on which tree, so `aaf run` can show why a
    /// gate run has not started yet. Library code never prints.
    pub progress: GateProgress,
    /// Where the factory creates task and integration worktrees. Only a
    /// tree below it is gated with the strict policy, which deletes files.
    pub worktrees_dir: PathBuf,
}

/// Receives the tree being gated and what the machine throttle reports.
pub type GateProgress = Box<dyn Fn(&Path, &Progress<'_>) + Send + Sync>;

impl RealGate {
    /// True when `tree` is strictly below the directory the factory creates
    /// its worktrees in. Both paths are resolved first; one that cannot be
    /// resolved is not the factory's.
    fn made_by_the_factory(&self, tree: &Path) -> bool {
        let (Ok(tree), Ok(root)) = (
            std::fs::canonicalize(tree),
            std::fs::canonicalize(&self.worktrees_dir),
        ) else {
            return false;
        };
        tree != root && tree.starts_with(&root)
    }
}

impl GateRunner for RealGate {
    fn run(&self, tree: &Path) -> PortResult<Receipt> {
        // A worktree the factory made holds what an agent wrote: files it
        // hid from the fingerprint (`.git/info/exclude`, a global excludes
        // file, a self-ignoring `.gitignore`) are deleted before and after
        // the commands, so they can neither change the result nor ride
        // along unsigned. Deleting is right only there. Every tree this
        // adapter is handed today is such a worktree (task and integration
        // trees of `gate_batch`, the rebased task tree of the merge queue);
        // the path test below keeps it so if a caller ever passes anything
        // else, the operator's own checkout above all: that gets the
        // default policy, which only warns.
        let run = if self.made_by_the_factory(tree) {
            gate::run_gate_strict
        } else {
            gate::run_gate_observed
        };
        // Heavy commands wait here for a machine slot, memory and CPU load
        // (PLAN C.11). That wait is part of the call, never an error.
        run(
            &self.data_dir,
            tree,
            &self.cfg,
            GateMode::Scoped,
            &*self.clock,
            &self.machine,
            &|event| (self.progress)(tree, event),
        )
        .map_err(|e| PortError::Io(e.to_string()))
    }

    fn head_lock(&self, tree: &Path) -> PortResult<HeadLockReport> {
        let base = gate::base::resolve(tree, &self.cfg.gate.base_ref, gate::GIT_TIMEOUT)
            .map_err(|e| PortError::Io(e.to_string()))?;
        Ok(gate::head_lock::evaluate(
            &self.data_dir,
            tree,
            &self.cfg,
            &base.diff_target,
        ))
    }

    fn verify_cached_receipt(&self, tree: &Path, receipt: &Receipt) -> bool {
        if receipt.status != GateStatus::Clear || receipt.schema_version != RECEIPT_SCHEMA_VERSION {
            return false;
        }
        let Ok(canon) = std::fs::canonicalize(tree) else {
            return false;
        };
        if Path::new(&receipt.repo_root) != canon {
            return false;
        }
        let expected_cfg = gate::config_hash(&self.cfg);
        if receipt.config_hash != expected_cfg {
            return false;
        }
        let Ok(base) = gate::base::resolve(&canon, &self.cfg.gate.base_ref, gate::GIT_TIMEOUT)
        else {
            return false;
        };
        if receipt.base_sha.as_deref() != base.sha.as_deref() {
            return false;
        }
        let Ok(hidden) = aaf_fp::hidden_files(&canon, gate::GIT_TIMEOUT) else {
            return false;
        };
        if !hidden.flagged.is_empty() || !hidden.ignored.is_empty() {
            return false;
        }
        let Ok(current_fp) = aaf_fp::tree_fp(&canon, gate::GIT_TIMEOUT) else {
            return false;
        };
        if current_fp != receipt.tree_fp {
            return false;
        }
        if self.cfg.proof.required
            && crate::proof::verify_proof(
                &canon,
                &self.data_dir,
                &self.cfg.proof,
                Some(&current_fp),
            )
            .is_err()
        {
            return false;
        }
        let Ok(key) = aaf_fp::load_key(&self.data_dir) else {
            return false;
        };
        let Ok(stored) = aaf_fp::read_receipt(&self.data_dir, &key, &canon) else {
            return false;
        };
        stored.status == GateStatus::Clear
            && stored.schema_version == RECEIPT_SCHEMA_VERSION
            && Path::new(&stored.repo_root) == canon
            && stored.tree_fp == current_fp
            && stored.config_hash == expected_cfg
            && stored.base_sha == receipt.base_sha
    }

    fn tree_binding(&self, tree: &Path) -> Option<(String, Option<String>)> {
        let fp = aaf_fp::tree_fp(tree, gate::GIT_TIMEOUT).ok()?;
        let base = gate::base::resolve(tree, &self.cfg.gate.base_ref, gate::GIT_TIMEOUT).ok()?;
        Some((fp, base.sha))
    }

    fn rerun_failed_test(&self, tree: &Path, receipt: &Receipt) -> PortResult<Option<RerunResult>> {
        if !is_pure_test_failure(receipt) {
            return Ok(None);
        }
        let Some(failed) = receipt
            .commands
            .iter()
            .find(|c| !c.passed() && matches!(c.name.as_str(), "test_scoped" | "test_full"))
        else {
            return Ok(None);
        };
        // Always take the command string from the main checkout's RepoConfig,
        // never from an untrusted Receipt payload.
        let command = match failed.name.as_str() {
            "test_scoped" => self.cfg.commands.test_scoped.trim(),
            "test_full" => self.cfg.commands.test_full.trim(),
            _ => return Ok(None),
        };
        if command.is_empty() {
            return Ok(None);
        }

        let repo_root = std::fs::canonicalize(tree).map_err(|e| PortError::Io(e.to_string()))?;
        private::create_dir(&self.data_dir).map_err(|e| PortError::Io(e.to_string()))?;
        let base = gate::base::resolve(&repo_root, &self.cfg.gate.base_ref, gate::GIT_TIMEOUT)
            .map_err(|e| PortError::Io(e.to_string()))?;
        let set = gate::changes::collect(&repo_root, &base.diff_target, gate::GIT_TIMEOUT)
            .map_err(|e| PortError::Io(e.to_string()))?;
        let diff_env = DiffEnvFiles::write(receipt.mode, base.sha.as_deref(), &set.changes)
            .map_err(|e| PortError::Io(e.to_string()))?;
        let env_owned = diff_env.env_pairs();
        let env_refs: Vec<(&str, &str)> = env_owned.iter().map(|(k, v)| (*k, v.as_str())).collect();

        let timeout = Duration::from_secs(self.cfg.gate.command_timeout_secs);
        let stats_id = gate::stats_repo_id(&repo_root);
        let probe = SystemProbe::detect();
        let progress = |event: &Progress<'_>| (self.progress)(tree, event);
        let throttle = Throttle::system(&self.machine, &probe, &progress);
        let heavy = HeavyContext {
            data_dir: &self.data_dir,
            stats_repo_id: &stats_id,
            clock: &*self.clock,
            throttle: &throttle,
        };
        let policy = if self.made_by_the_factory(tree) {
            HiddenPolicy::Remove
        } else {
            HiddenPolicy::default()
        };

        let mut runs = 0u32;
        let mut passed = 0u32;
        for _ in 0..2 {
            let before = gate::hidden::inspect(&repo_root, policy, When::BeforeCommands, false)
                .map_err(|e| PortError::Io(e.to_string()))?;
            if !before.blocks.is_empty() {
                return Ok(None);
            }
            let result = gate::runner::run_one_heavy(
                &repo_root,
                &failed.name,
                command,
                timeout,
                &heavy,
                &env_refs,
            )
            .map_err(|e| PortError::Io(e.to_string()))?;
            if policy == HiddenPolicy::Remove {
                let _ = gate::hidden::inspect(&repo_root, policy, When::AfterCommands, false)
                    .map_err(|e| PortError::Io(e.to_string()))?;
            }
            runs += 1;
            // One pass is all the classifier needs (`passed > 0`): a second
            // run of a slow suite would only cost time.
            if result.passed() {
                passed += 1;
                break;
            }
            if result.timed_out {
                break;
            }
        }
        Ok(Some(RerunResult { runs, passed }))
    }
}

/// The provider whose accounts an executor (`[run].executor` or
/// `[cross_audit].executor`) can use; `None` for `custom` commands, which run
/// without a discovered account profile.
pub fn provider_for(executor: &str) -> Option<ProviderKind> {
    match executor.trim().to_ascii_lowercase().as_str() {
        "claude" => Some(ProviderKind::ClaudeCode),
        "codex" => Some(ProviderKind::Codex),
        "antigravity" | "gemini" => Some(ProviderKind::Antigravity),
        "opencode" => Some(ProviderKind::OpenCode),
        _ => None,
    }
}

/// The account table narrowed to one provider. The router has no provider
/// filter of its own, and handing a Codex account to the Claude runner is a
/// hard error there.
struct ProviderView<'a> {
    inner: &'a dyn AccountStore,
    provider: Option<ProviderKind>,
}

impl AccountStore for ProviderView<'_> {
    fn upsert_account(&self, profile: &AccountProfile) -> PortResult<()> {
        self.inner.upsert_account(profile)
    }

    fn list_accounts(&self) -> PortResult<Vec<AccountState>> {
        let mut accounts = self.inner.list_accounts()?;
        accounts.retain(|state| Some(state.profile.provider) == self.provider);
        Ok(accounts)
    }

    fn set_cooldown(&self, account_id: &str, until: Option<i64>) -> PortResult<()> {
        self.inner.set_cooldown(account_id, until)
    }
}

/// [`AccountPool`] on the router and its file leases.
pub struct RouterPool {
    pub accounts: Arc<dyn AccountStore>,
    pub runs: Arc<dyn RunStore>,
    pub clock: Arc<dyn Clock>,
    /// `<data dir>/leases`.
    pub leases_dir: PathBuf,
    pub max_concurrent_per_account: u32,
    pub provider: Option<ProviderKind>,
}

impl RouterPool {
    fn with_router<T>(&self, f: impl FnOnce(&Router<'_>) -> PortResult<T>) -> PortResult<T> {
        let view = ProviderView {
            inner: &*self.accounts,
            provider: self.provider,
        };
        f(&Router::new(
            &view,
            &*self.runs,
            &*self.clock,
            self.leases_dir.clone(),
            self.max_concurrent_per_account,
        ))
    }

    /// The accounts of this pool's provider, with their cooldowns.
    pub fn provider_accounts(&self) -> PortResult<Vec<AccountState>> {
        ProviderView {
            inner: &*self.accounts,
            provider: self.provider,
        }
        .list_accounts()
    }

    /// The least-used free account among those `allow` accepts, leased.
    /// `None` when none of them is free: the caller waits.
    pub fn acquire_where(
        &self,
        allow: &dyn Fn(&AccountProfile) -> bool,
    ) -> PortResult<Option<Grant>> {
        if self.provider_accounts()?.is_empty() {
            // No account of this provider at all: the agent runs on its
            // default login and nothing is leased.
            return Ok(Some(Grant::new(None, None)));
        }
        self.with_router(|router| {
            let next = router
                .next_where(allow)
                .map_err(|e| PortError::Io(e.to_string()))?;
            Ok(next.map(|(profile, lease)| Grant::new(Some(profile), Some(Box::new(lease)))))
        })
    }
}

impl AccountPool for RouterPool {
    fn acquire(&self) -> PortResult<Option<Grant>> {
        self.acquire_where(&|_| true)
    }

    fn report_limit(&self, account_id: &str, hit: &LimitHit) -> PortResult<()> {
        self.with_router(|router| {
            router
                .report_limit(account_id, hit)
                .map_err(|e| PortError::Io(e.to_string()))
        })
    }

    fn profiles(&self) -> PortResult<Vec<AccountProfile>> {
        // Every provider: the sandbox closes all of them but the run's own.
        Ok(self
            .accounts
            .list_accounts()?
            .into_iter()
            .map(|state| state.profile)
            .collect())
    }

    fn provider_account_count(&self) -> PortResult<usize> {
        if self.provider.is_none() {
            return Ok(0);
        }
        Ok(self.provider_accounts()?.len())
    }
}

/// Posts T5 escalations to `notifications.webhook_url` using a shared reusable
/// HTTP agent and tighter factory-worker timeouts so dead endpoints do not
/// stall workers.
pub struct WebhookNotifier {
    pub url: String,
}

impl WebhookNotifier {
    /// Tighter non-blocking-friendly delivery options for factory workers
    /// (`connect_timeout: 3s`, `timeout: 5s`, `max_retries: 1`).
    pub fn factory_options() -> WebhookOptions {
        WebhookOptions {
            connect_timeout: Duration::from_secs(3),
            timeout: Duration::from_secs(5),
            max_retries: 1,
            ..WebhookOptions::default()
        }
    }

    fn shared_agent() -> &'static ureq::Agent {
        static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
        AGENT.get_or_init(|| webhook::build_agent(&Self::factory_options()))
    }
}

impl Notifier for WebhookNotifier {
    fn notify(&self, event: &WebhookEvent) -> Result<bool, String> {
        if self.url.trim().is_empty() {
            return Ok(false);
        }
        let opts = Self::factory_options();
        webhook::send_with_agent(Self::shared_agent(), &self.url, event, &opts)
            .map(|()| true)
            .map_err(|e| e.to_string())
    }
}
