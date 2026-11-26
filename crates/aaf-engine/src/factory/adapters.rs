//! Production implementations of the factory's ports on the engine modules.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

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

pub use crate::clock::SystemClock;

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
    /// True when `tree` is strictly below `self.worktrees_dir` OR is a
    /// verified non-symlink AAF-managed worktree registered on an `aaf/<id>`
    /// branch of its repository (e.g. reused after `AAF_WORKTREE_DIR` changed).
    fn made_by_the_factory(&self, tree: &Path) -> bool {
        let Ok(canon_tree) = std::fs::canonicalize(tree) else {
            return false;
        };
        if let Ok(root) = std::fs::canonicalize(&self.worktrees_dir)
            && canon_tree != root
            && canon_tree.starts_with(&root)
        {
            return true;
        }
        if !crate::git::worktree::is_non_symlink_dir(tree) {
            return false;
        }
        let main_repo = crate::git::main_work_tree(&canon_tree);
        let Ok(entries) = crate::git::worktree::list(&main_repo) else {
            return false;
        };
        entries.iter().skip(1).any(|entry| {
            let Some(task_id) = entry.task_id() else {
                return false;
            };
            std::fs::canonicalize(&entry.path).is_ok_and(|p| p == canon_tree)
                && crate::git::worktree::worktree_path(&main_repo, &self.worktrees_dir, task_id)
                    .is_ok_and(|expected| {
                        crate::git::worktree::is_aaf_managed_worktree_path(
                            &entry.path,
                            &expected,
                            &entries,
                        )
                    })
        })
    }
}

impl RealGate {
    fn run_mode(&self, tree: &Path, mode: GateMode) -> PortResult<Receipt> {
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
            mode,
            &*self.clock,
            &self.machine,
            &|event| (self.progress)(tree, event),
        )
        .map_err(|e| PortError::Io(e.to_string()))
    }
}

impl GateRunner for RealGate {
    fn run(&self, tree: &Path) -> PortResult<Receipt> {
        self.run_mode(tree, GateMode::Scoped)
    }

    fn run_full(&self, tree: &Path) -> PortResult<Receipt> {
        self.run_mode(tree, GateMode::Full)
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
        let repo_root = std::fs::canonicalize(tree).map_err(|e| PortError::Io(e.to_string()))?;
        private::create_dir(&self.data_dir).map_err(|e| PortError::Io(e.to_string()))?;
        let base = gate::base::resolve(&repo_root, &self.cfg.gate.base_ref, gate::GIT_TIMEOUT)
            .map_err(|e| PortError::Io(e.to_string()))?;
        let set = gate::changes::collect(&repo_root, &base.diff_target, gate::GIT_TIMEOUT)
            .map_err(|e| PortError::Io(e.to_string()))?;
        // Always take the command string from the main checkout's RepoConfig,
        // never from an untrusted Receipt payload, with the same
        // `{affected_packages}` the gate run had: the bare placeholder is a
        // test-name filter that matches nothing and passes.
        let commands =
            gate::rust_scope::expanded_cfg(&self.cfg, receipt.mode, &repo_root, &set.changes);
        let command = match failed.name.as_str() {
            "test_scoped" => commands.commands.test_scoped.trim(),
            "test_full" => commands.commands.test_full.trim(),
            _ => return Ok(None),
        };
        if command.is_empty() || command.contains(gate::rust_scope::AFFECTED_PACKAGES) {
            return Ok(None);
        }
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
            if result.passed() {
                passed += 1;
                // A single passing rerun already proves non-determinism
                // (`classifier` checks `passed > 0`); stop immediately instead
                // of paying for a second redundant test run.
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
