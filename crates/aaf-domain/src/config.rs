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
    pub cross_audit: GlobalCrossAuditConfig,
    pub telegram: crate::telegram::TelegramConfig,
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
    /// Optional machine-wide cap on open Review + Running + Gating tasks when
    /// `auto_merge = false`. Positive tier-1 caps cannot be loosened by repo
    /// or local config.
    pub max_open_review: Option<u32>,
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
    /// Real CPU busy percentage ceiling (0..=100) verified via OS CPU counters
    /// when load average exceeds `max_load_percent`. Default 85 prevents macOS
    /// I/O-inflated `vm.loadavg` from falsely blocking builds while real CPU
    /// has headroom. Set 0 to disable real-CPU verification and rely solely on
    /// `max_load_percent`.
    pub max_cpu_percent: u32,
    /// Longest wait for the CPU load to drop. After it the command runs anyway
    /// (at low priority): load alone never stalls a task. Memory pressure and
    /// a taken slot are waited out without this ceiling, because every slot
    /// holder is bounded by its own command timeout.
    pub heavy_wait_secs: u64,
    /// Run heavy commands at low CPU and I/O priority so the desktop stays
    /// responsive.
    pub low_priority: bool,
    /// When true (default), Gradle, Unity and xcodebuild commands always run
    /// exclusively across all heavy slots regardless of recorded peak memory.
    /// When false, measured runs share slots once free memory covers 1.25x
    /// their recorded peak RSS (unmeasured first runs still run alone, and
    /// same-worktree / Unity-editor correctness locks still apply).
    pub exclusive_build_tools: bool,
    /// Machine-wide cap on concurrent Unity Editor batch runs when
    /// `exclusive_build_tools = false`. Default 1 prevents Unity License/Library
    /// contention without blocking Gradle or other build tools.
    pub max_unity_parallel: u32,
    /// Load percentage ceiling (100 = 1.0x CPU count) used by `aaf gate --when-idle`.
    /// 0 disables the load check in idle mode.
    pub idle_max_load_percent: u32,
}

impl Default for MachineConfig {
    fn default() -> Self {
        MachineConfig {
            max_heavy_parallel: 0,
            max_load_percent: 150,
            max_cpu_percent: 85,
            heavy_wait_secs: 1800,
            low_priority: true,
            exclusive_build_tools: true,
            max_unity_parallel: 1,
            idle_max_load_percent: 100,
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
    pub cross_audit: CrossAuditConfig,
    pub proof: ProofConfig,
    pub github: GithubBoardConfig,
    pub pipeline: PipelineConfig,
}

/// Hard upper bound on automatic audit-rework rounds before escalating.
pub const MAX_AUDIT_REWORKS_CAP: u32 = 5;

/// Optional multi-stage pipeline settings (plan stage & automatic audit rework).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PipelineConfig {
    /// Generate a read-only plan before the first implementation turn.
    pub plan_stage: bool,
    /// Pause for explicit human approval (`aaf board approve-plan`) after plan generation.
    pub require_plan_approval: bool,
    /// Maximum automatic rework rounds after an explicit `AuditVerdict::Fail` (capped at 5).
    pub max_audit_reworks: u32,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        PipelineConfig {
            plan_stage: false,
            require_plan_approval: false,
            max_audit_reworks: 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GithubBoardConfig {
    pub enabled: bool,
    pub repo: String,
    pub project_owner: String,
    pub project_number: Option<u32>,
    pub label_filter: String,
    pub columns: GithubColumnMap,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GithubColumnMap {
    pub backlog: String,
    pub ready: String,
    pub running: String,
    pub gating: String,
    pub review: String,
    pub blocked: String,
    pub done: String,
    pub cancelled: String,
}

impl Default for GithubColumnMap {
    fn default() -> Self {
        Self {
            backlog: "Backlog".to_string(),
            ready: "Ready".to_string(),
            running: "Building".to_string(),
            gating: "QA".to_string(),
            review: "Review".to_string(),
            blocked: "Blocked".to_string(),
            done: "Done".to_string(),
            cancelled: "Done".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProofConfig {
    pub required: bool,
    pub min_bytes: u64,
    pub min_width: u32,
    pub min_height: u32,
    pub denylist_file: String,
    pub allowed_tools: Vec<String>,
}

impl Default for ProofConfig {
    fn default() -> Self {
        Self {
            required: false,
            min_bytes: 8192,
            min_width: 100,
            min_height: 100,
            denylist_file: ".adb-denylist".to_string(),
            allowed_tools: vec!["mobile-mcp:mobile_save_screenshot".to_string()],
        }
    }
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
    /// Heavy project-wide lint run only in `--full` and `--release` modes.
    pub lint_full: String,
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
    /// Skip `lint` and `test_scoped` in `--scoped` mode when every changed
    /// path is a safe documentation file (still runs hidden-file, static,
    /// head-lock, store-compliance, and proof checks).
    pub docs_fast_path: bool,
    /// Extra path globs excluded from `docs_fast_path`.
    pub docs_fast_path_exclude_globs: Vec<String>,
}

/// What coding agents load from a repository as instructions, hooks, skills
/// and MCP servers. A task that could change them unaudited would change
/// its own guard (`.claude/settings.json` registers `aaf-hook`) and, once
/// merged, what runs in the owner's own sessions outside any sandbox.
/// Part of the default `head_lock_globs`.
pub const AGENT_CONFIG_GLOBS: [&str; 5] = [
    "**/.claude/**",
    "**/CLAUDE.md",
    "**/.mcp.json",
    "**/.agents/**",
    "**/AGENTS.md",
];

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
            "**/.adb-denylist",
            "**/.adb-allowlist",
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
            docs_fast_path: true,
            docs_fast_path_exclude_globs: Vec::new(),
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
    /// Queue brake when `auto_merge = false`: `0` disables the cap, positive
    /// values bound `Review + Running + Gating` tasks before starting more work.
    pub max_open_review: u32,
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
            max_open_review: 0,
        }
    }
}

/// When post-gate adversarial cross-audit runs beyond head-locked files.
/// Ordering is strictly monotonic: `HeadLock < Large < Always`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditScope {
    /// Only when head-locked files are touched (pre-gate approval).
    #[default]
    HeadLock,
    /// Head-locked files plus post-gate audit on large diffs (>= 3 files or > 200 prod LOC).
    Large,
    /// Head-locked files plus post-gate audit on every task.
    Always,
}

impl AuditScope {
    pub fn as_str(self) -> &'static str {
        match self {
            AuditScope::HeadLock => "head_lock",
            AuditScope::Large => "large",
            AuditScope::Always => "always",
        }
    }
}

/// Tier-1 machine-wide cross-audit defaults/floors. Numeric/boolean/scope fields
/// are `Option<...>` so unspecified tier-1 defaults never clobber explicit tier-2 values.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GlobalCrossAuditConfig {
    pub executor: String,
    pub executor_cmd: String,
    pub model: String,
    pub effort: String,
    pub require_different_provider: Option<bool>,
    pub audit_scope: Option<AuditScope>,
    pub score_gate: Option<bool>,
    pub min_average_score: Option<u8>,
    pub min_criterion_score: Option<u8>,
}

/// Default minimum average score (/100) required when `score_gate = true`.
pub const DEFAULT_MIN_AVERAGE_SCORE: u8 = 95;
/// Default minimum per-criterion score (/100) across G1..G7 when `score_gate = true`.
pub const DEFAULT_MIN_CRITERION_SCORE: u8 = 90;

/// Optional separate provider/model and post-gate scoring rules for adversarial cross-audit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CrossAuditConfig {
    /// Empty means reuse `[run].executor`. One of: claude, codex, gemini, opencode, custom.
    pub executor: String,
    /// Command template when `executor` is `custom` (or override binary path).
    pub executor_cmd: String,
    /// Optional model override for the cross-audit run.
    pub model: String,
    /// Optional reasoning effort override for the cross-audit run.
    pub effort: String,
    /// Fail closed instead of falling back to `same_account = true` when no
    /// auditor from a different provider is configured.
    pub require_different_provider: bool,
    /// Scope of cross-audit (`head_lock`, `large`, `always`).
    pub audit_scope: AuditScope,
    /// Require 7 Golden Standards (`G1..G7`) + `MechanicalScore` thresholds to pass.
    pub score_gate: bool,
    /// Minimum required `min(mechanical_score, llm_avg)` in `0..=100` (default 95).
    pub min_average_score: u8,
    /// Minimum required score per golden criterion `G1..G7` in `0..=100` (default 90).
    pub min_criterion_score: u8,
}

impl Default for CrossAuditConfig {
    fn default() -> Self {
        CrossAuditConfig {
            executor: String::new(),
            executor_cmd: String::new(),
            model: String::new(),
            effort: String::new(),
            require_different_provider: false,
            audit_scope: AuditScope::HeadLock,
            score_gate: false,
            min_average_score: DEFAULT_MIN_AVERAGE_SCORE,
            min_criterion_score: DEFAULT_MIN_CRITERION_SCORE,
        }
    }
}
