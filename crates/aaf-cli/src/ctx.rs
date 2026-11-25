//! Everything a command reads from the process environment, read once.
//!
//! Commands take a `&Ctx` instead of calling `std::env` so tests can build
//! one by hand (edition 2024 makes `set_var` unsafe, and unsafe is forbidden).

use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use aaf_domain::ports::{AccountStore, Clock};
use aaf_engine::config::{self, ConfigInputs, ConfigOverlay, EffectiveConfig};
use aaf_engine::db::Db;
use aaf_engine::telemetry::{CostPolicy, CostRates, cost_policy_for};

use crate::output::CmdError;

#[derive(Debug, Clone)]
pub struct Ctx {
    pub cwd: PathBuf,
    pub home: PathBuf,
    /// `AAF_DATA_DIR` or its default (PLAN C.1).
    pub data_dir: PathBuf,
    /// `AAF_CONFIG_DIR` or `~/.config/autofactory`.
    pub config_dir: PathBuf,
    /// `AAF_WORKTREE_DIR` or `~/.cache/autofactory/worktrees`.
    pub worktrees_dir: PathBuf,
    pub path_var: OsString,
    /// `AAF_RUN_ID`: set when this process was started by an agent run.
    pub run_id: Option<String>,
    pub stdin_is_tty: bool,
    /// All `AAF_*` variables, for the config loader's env tier.
    pub aaf_env: Vec<(String, String)>,
}

/// What `git rev-parse --show-toplevel` in the current directory amounts to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoState {
    Root(PathBuf),
    NotARepo,
    /// A repository is there and git failed on it; the text says what to do.
    GitFailed(String),
}

impl Ctx {
    pub fn from_process_env() -> Result<Ctx, CmdError> {
        let cwd = std::env::current_dir().map_err(|e| CmdError::new(format!("cwd: {e}")))?;
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| CmdError::new("HOME is not set"))?;
        let data_dir = aaf_fp::data_dir().map_err(|e| CmdError::new(format!("data dir: {e}")))?;
        let worktrees_dir = std::env::var_os("AAF_WORKTREE_DIR")
            .filter(|v| !v.is_empty())
            // The default `aaf_engine::executors::ExecEnv` assumes as well.
            .map_or_else(
                || home.join(aaf_engine::executors::DEFAULT_WORKTREES_REL),
                PathBuf::from,
            );
        // Same rule as `aaf_engine::config::global_config_path`.
        let config_dir = std::env::var_os("AAF_CONFIG_DIR")
            .filter(|v| !v.is_empty())
            .map_or_else(|| home.join(".config/autofactory"), PathBuf::from);
        Ok(Ctx {
            cwd,
            config_dir,
            home,
            data_dir,
            worktrees_dir,
            path_var: std::env::var_os("PATH").unwrap_or_default(),
            run_id: std::env::var("AAF_RUN_ID").ok().filter(|v| !v.is_empty()),
            stdin_is_tty: std::io::stdin().is_terminal(),
            aaf_env: std::env::vars()
                .filter(|(k, _)| k.starts_with("AAF_"))
                .collect(),
        })
    }

    /// Whether the current directory is in a git work tree, and when git
    /// cannot say, whether one is there all the same.
    pub fn repo_state(&self) -> RepoState {
        let failure = match aaf_fp::repo_root(&self.cwd) {
            Ok(root) => return RepoState::Root(root),
            Err(failure) => failure,
        };
        // git says "not a repository" in the same breath as "I refuse this
        // one" (dubious ownership, a broken `.git`, a timeout). A `.git`
        // entry here or above tells them apart without parsing its words.
        match self.cwd.ancestors().find(|dir| dir.join(".git").exists()) {
            None => RepoState::NotARepo,
            Some(dir) => RepoState::GitFailed(format!(
                "git could not read the repository at {dir} although {dir}/.git exists \
                 ({failure}): run `git -C {dir} status` to see why (an owner git does not \
                 trust: git config --global --add safe.directory {dir}; a broken .git; or git \
                 timed out)",
                dir = dir.display()
            )),
        }
    }

    /// Top level of the git work tree containing the current directory.
    pub fn repo_root(&self) -> Result<PathBuf, CmdError> {
        match self.repo_state() {
            RepoState::Root(root) => Ok(root),
            RepoState::NotARepo => Err(CmdError::new(format!(
                "{} is not inside a git work tree",
                self.cwd.display()
            ))),
            RepoState::GitFailed(why) => Err(CmdError::new(why)),
        }
    }

    /// The three config tiers plus the `AAF_*` environment, for `repo_root`.
    pub fn load_config(&self, repo_root: &Path) -> Result<EffectiveConfig, CmdError> {
        let env = |key: &str| {
            self.aaf_env
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        };
        config::load(&ConfigInputs {
            global_config: Some(self.config_dir.join("config.toml")),
            repo_root: repo_root.to_path_buf(),
            env: &env,
            cli: ConfigOverlay::default(),
        })
        .map_err(|e| CmdError::new(format!("config: {e}")))
    }

    /// Builds a [`CostPolicy`] from tier-1 `[accounts.billing]` and persisted accounts.
    /// Any config or DB error falls back to an empty policy (omitting USD safely).
    pub fn cost_policy(&self, db: &Db) -> CostPolicy {
        let Ok(global) = config::load_global(&self.config_dir) else {
            return CostPolicy::default();
        };
        let Ok(accounts) = db.list_accounts() else {
            return CostPolicy::default();
        };
        cost_policy_for(
            &global.accounts,
            &self.home,
            &accounts,
            CostRates::default(),
        )
    }
}

/// Path of the running `aaf` binary, to find `aaf-hook` installed next to it.
pub fn current_exe() -> Option<PathBuf> {
    std::env::current_exe().ok()
}

/// True when `--json` is among the raw arguments. Only for the case where
/// clap rejected the command line, so no parsed flag exists.
pub fn json_requested() -> bool {
    std::env::args_os().any(|arg| arg == "--json")
}

/// Wall clock for the commands; tests of the engine use their own fakes.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
    }
}
