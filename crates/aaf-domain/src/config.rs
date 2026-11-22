//! Configuration types for the three tiers (global, repo, local override).
//!
//! Parsing and tier merging live in `aaf-engine::config`; this module only
//! defines the shapes and their defaults.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Tier 1: `~/.config/autofactory/config.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GlobalConfig {
    pub accounts: AccountsConfig,
    pub safety: SafetyConfig,
    pub notifications: NotificationsConfig,
    pub machine: MachineConfig,
    pub run: MachineRunConfig,
}

/// The part of `[run]` the operator may also set for the whole machine.
/// Nothing else of `[run]` is read from tier 1: the executor and its limits
/// belong to the repository.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MachineRunConfig {
    /// Names of environment variables handed to the agents of every
    /// repository, on top of the built-in allow-list and of the
    /// repository's own `run.env_pass`. This file is outside every worktree,
    /// so no task can add a name to it.
    pub env_pass: Vec<String>,
}

/// Machine-wide limits on heavy gate commands (Gradle, Unity, Xcode builds),
/// shared by every repo and every `aaf` process of this user. Tier 1 only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MachineConfig {
    /// Ceiling on gate build/test commands running at the same time on this
    /// machine. 0 = automatic: a quarter of the CPUs (at least 1). Under the
    /// ceiling a second command is admitted only when free memory covers its
    /// recorded peak; a command never measured before runs alone.
    pub max_heavy_parallel: u32,
    /// A heavy command waits while the 1-minute load average exceeds this
    /// percentage of the CPU count, or while memory pressure is not normal.
    /// 0 disables the load check.
    pub max_load_percent: u32,
    /// Longest wait for the CPU load to drop. After it the command runs anyway
    /// (at low priority): load alone never stalls a task. Memory pressure and
    /// a taken slot are waited out without this ceiling, because every slot
    /// holder is bounded by its own command timeout.
    pub heavy_wait_secs: u64,
    /// Run heavy commands at low CPU and I/O priority so the desktop stays
    /// responsive.
    pub low_priority: bool,
}

impl Default for MachineConfig {
    fn default() -> Self {
        MachineConfig {
            max_heavy_parallel: 0,
            max_load_percent: 150,
            heavy_wait_secs: 1800,
            low_priority: true,
        }
    }
}

/// How a job gets its account (PLAN C.8). Tier 1 only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountSelection {
    /// One account per provider for a repository. While that account is
    /// cooling down or leased the job waits; it never moves to another
    /// account of the same provider.
    #[default]
    Pinned,
    /// A task keeps its account and a repository prefers its last one, but
    /// a limit hit or a busy lease moves to another account, which is then
    /// kept.
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccountsConfig {
    pub discovery_globs: Vec<String>,
    pub max_concurrent_per_account: u32,
    pub selection: AccountSelection,
    /// Display names: config dir path (a leading `~` is the home dir) to the
    /// name shown for that account. Never changes an account id.
    pub aliases: BTreeMap<String, String>,
}

impl Default for AccountsConfig {
    fn default() -> Self {
        AccountsConfig {
            discovery_globs: vec!["~/.claude".to_string(), "~/.claude-*".to_string()],
            max_concurrent_per_account: 1,
            selection: AccountSelection::default(),
            aliases: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SafetyConfig {
    /// GitHub logins whose issues and comments may become tasks.
    /// Empty means nobody is trusted.
    pub owner_github_allowlist: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotificationsConfig {
    /// Generic JSON POST target; empty disables notifications.
    pub webhook_url: String,
}

/// Tier 2: `.autofactory/config.toml`, committed to the repo.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RepoConfig {
    pub project: ProjectConfig,
    pub commands: CommandsConfig,
    pub gate: GateConfig,
    pub unblocker: UnblockerConfig,
    pub run: RunConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProjectConfig {
    pub name: String,
    /// `auto` or one of: android, ios, flutter, react-native, expo, web,
    /// backend, rust, unity.
    pub profile: String,
}

impl Default for ProjectConfig {
    fn default() -> Self {
        ProjectConfig {
            name: String::new(),
            profile: "auto".to_string(),
        }
    }
}

/// Shell commands the gate runs. Read from tier 2 only.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CommandsConfig {
    pub build: String,
    pub test_scoped: String,
    pub test_full: String,
    pub lint: String,
    /// A command that reproduces a reported failure. Read and stored, but
    /// nothing runs it yet: `aaf_engine::detector::repro` has no caller.
    pub repro: String,
}

/// Ordered so that a higher tier may only raise it: `Off < Auto < Always`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum StoreMode {
    Off,
    #[default]
    Auto,
    Always,
}

impl StoreMode {
    pub fn parse(s: &str) -> Option<StoreMode> {
        match s.to_ascii_lowercase().as_str() {
            "off" => Some(StoreMode::Off),
            "auto" => Some(StoreMode::Auto),
            "always" => Some(StoreMode::Always),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GateConfig {
    /// `auto` or an explicit ref such as `origin/main`.
    pub base_ref: String,
    /// Existing files matching these globs may not be modified, deleted or
    /// renamed without a signed approval. Newly added files are exempt,
    /// except those matching [`AGENT_CONFIG_GLOBS`].
    pub head_lock_globs: Vec<String>,
    pub store_compliance: StoreMode,
    pub min_android_target_sdk: u32,
    pub command_timeout_secs: u64,
}

/// What Claude Code loads from a repository as instructions, hooks, agents
/// and MCP servers. A task that could change them unaudited would change
/// its own guard (`.claude/settings.json` registers `aaf-hook`) and, once
/// merged, what runs in the owner's own sessions outside any sandbox.
/// Part of the default `head_lock_globs`.
pub const AGENT_CONFIG_GLOBS: [&str; 3] = ["**/.claude/**", "**/CLAUDE.md", "**/.mcp.json"];

/// Google Play requirement for new apps and updates since 2026-08-31.
pub const DEFAULT_MIN_ANDROID_TARGET_SDK: u32 = 36;

impl Default for GateConfig {
    fn default() -> Self {
        let globs = [
            "**/*_test.*",
            "**/*.test.*",
            "**/*.spec.*",
            "**/test_*.py",
            "**/*Test.kt",
            "**/*Test.java",
            "**/*Tests.swift",
            "**/*Tests.cs",
            "**/tests/**",
            "**/test/**",
            "**/__tests__/**",
            "**/androidTest/**",
            "**/jest.config.*",
            "**/vitest.config.*",
            "**/pytest.ini",
            "**/conftest.py",
            ".github/workflows/**",
            ".autofactory/config.toml",
            "**/Cargo.toml",
            "**/package.json",
            "**/build.rs",
            "**/build.gradle",
            "**/build.gradle.kts",
        ];
        GateConfig {
            base_ref: "auto".to_string(),
            head_lock_globs: globs
                .iter()
                .chain(AGENT_CONFIG_GLOBS.iter())
                .map(|g| (*g).to_string())
                .collect(),
            store_compliance: StoreMode::Auto,
            min_android_target_sdk: DEFAULT_MIN_ANDROID_TARGET_SDK,
            command_timeout_secs: 1800,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UnblockerConfig {
    pub max_unblock_cycles: u32,
    pub max_subtask_depth: u8,
    pub max_token_budget: u64,
    pub max_wall_clock_secs: u64,
    pub global_freeze_consecutive_fails: u32,
}

impl Default for UnblockerConfig {
    fn default() -> Self {
        UnblockerConfig {
            max_unblock_cycles: 5,
            max_subtask_depth: 1,
            max_token_budget: 500_000,
            max_wall_clock_secs: 3600,
            global_freeze_consecutive_fails: 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunConfig {
    /// `claude`, `codex`, `antigravity`, `opencode` or `custom`.
    pub executor: String,
    /// Command template for non-Claude executors; `{prompt}` is substituted.
    pub executor_cmd: Vec<String>,
    pub auto_merge: bool,
    pub max_parallel: u32,
    pub run_timeout_secs: u64,
    /// `--model` of the Claude executor; empty leaves the CLI's own default.
    pub model: String,
    /// `--effort` of the Claude executor; empty leaves the CLI's own default.
    pub effort: String,
    /// Names of environment variables handed to the agent on top of the
    /// built-in allow-list (`aaf_engine::executors`). Never from tier 3 or
    /// above: it widens what an agent can read. The file is committed, so a
    /// task can propose a name; it takes effect only once that change is on
    /// the base, and `.autofactory/config.toml` is head-locked (audit or a
    /// signed approval first). The operator's own list is
    /// [`MachineRunConfig::env_pass`] in tier 1.
    pub env_pass: Vec<String>,
}

impl Default for RunConfig {
    fn default() -> Self {
        RunConfig {
            executor: "claude".to_string(),
            executor_cmd: Vec::new(),
            auto_merge: false,
            max_parallel: 1,
            run_timeout_secs: 3600,
            model: String::new(),
            effort: String::new(),
            env_pass: Vec::new(),
        }
    }
}
