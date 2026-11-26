//! Configuration types for the three tiers (global, repo, local override).
//!
//! Parsing and tier merging live in `aaf-engine::config`; this module only
//! defines the shapes and their defaults.

use serde::{Deserialize, Serialize};

pub use crate::accounts_config::{AccountSelection, AccountsConfig, Billing};
pub use crate::land_mode::LandMode;
pub use crate::oracle_config::OracleMode;
pub use crate::pipeline::{
    DEFAULT_ESCALATE_IMPLEMENTER_AFTER_REWORKS, DEFAULT_MAX_PLAN_ROUNDS, GlobalPipelineConfig,
    GlobalUnblockerConfig, MAX_PLAN_ROUNDS_CAP, ReviewerConfig, StageModelConfig,
};
pub use crate::proof_flow_config::{GlobalProofConfig, ProofFlow, validate_flows};

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
    pub pipeline: GlobalPipelineConfig,
    pub unblocker: GlobalUnblockerConfig,
    pub telegram: crate::telegram::TelegramConfig,
    pub proof: GlobalProofConfig,
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
    /// Explicit Tier-1 allowlist of repo roots allowed to auto-land when Tier-2
    /// leaves `auto_merge`/`land` unset. Explicit Tier-2 `false`/`pr` wins.
    #[serde(default)]
    pub auto_merge_repos: Vec<String>,
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
    /// percentage of the CPU count and real CPU busy is at least
    /// `max_cpu_percent` (when measurable), or while memory pressure is not
    /// normal. 0 disables the load check.
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
    /// Machine-wide cap on concurrent Gradle builds when
    /// `exclusive_build_tools = false`. Default 2 permits cross-repo builds
    /// (e.g. two Android repos) without triggering 3+ JVM storms. `0` = uncapped.
    pub max_gradle_parallel: u32,
    /// Load percentage ceiling (100 = 1.0x CPU count) used by `aaf gate --when-idle`.
    /// 0 disables the load check in idle mode.
    pub idle_max_load_percent: u32,
    /// Extra Tier-1-only writable cache directories inside sandbox.
    #[serde(default)]
    pub writable_caches: Vec<String>,
}

impl Default for MachineConfig {
    fn default() -> Self {
        MachineConfig {
            max_heavy_parallel: 0,
            max_load_percent: 150,
            max_cpu_percent: 85,
            heavy_wait_secs: 300,
            low_priority: true,
            exclusive_build_tools: true,
            max_unity_parallel: 1,
            max_gradle_parallel: 2,
            idle_max_load_percent: 100,
            writable_caches: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SafetyConfig {
    /// GitHub logins whose issues and comments may become tasks.
    /// Empty means nobody is trusted.
    pub owner_github_allowlist: Vec<String>,
    /// macOS Seatbelt Keychain read/IPC confinement (`auto`, `always`, `never`).
    pub deny_macos_keychain: crate::keychain::KeychainPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotificationsConfig {
    /// Generic JSON POST target; empty disables notifications.
    pub webhook_url: String,
    /// Optional Tier-1 HTTPS public origin of the Cockpit (e.g. Tailscale Serve
    /// `https://<machine>.<tailnet>.ts.net`). Used by `aaf ui --service` as a
    /// trusted reverse-proxy origin and by `aaf notify` for Telegram deep-links.
    pub cockpit_url: String,
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
    pub full_gate: FullGateConfig,
    #[serde(
        default,
        skip_serializing_if = "crate::worktree_config::WorktreeConfig::is_empty"
    )]
    pub worktree: crate::worktree_config::WorktreeConfig,
}

/// The full gate `aaf run --auto` runs on the base branch by itself. Both
/// `0` (the baseline here) turn it off; `defaults.toml` sets the real values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FullGateConfig {
    /// Hours that must pass after a full run before the next one, which then
    /// starts only while no task is Ready, Running or Gating. `0` = off.
    pub cooldown_hours: u64,
    /// After this many days without a full run it starts even while tasks
    /// are running (its commands still wait for a machine slot). `0` = only
    /// when idle.
    pub max_days: u64,
}

/// Hard upper bound on automatic audit-rework rounds before escalating.
pub const MAX_AUDIT_REWORKS_CAP: u32 = 5;

/// Optional multi-stage pipeline settings (plan stage, debate, tiered implementer & rework).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PipelineConfig {
    /// Generate a read-only plan before the first implementation turn.
    pub plan_stage: bool,
    /// Enable adversarial cross-provider Plan Debate before implementation.
    pub plan_debate: bool,
    /// Pause for explicit human approval (`aaf board approve-plan`) after plan generation.
    pub require_plan_approval: bool,
    /// Maximum Plan Debate rounds before escalating (capped at [`MAX_PLAN_ROUNDS_CAP`]).
    pub max_plan_rounds: u32,
    /// Allow deterministic Tier-S safe docs tasks to skip Plan Debate.
    pub allow_tier_s_skip_debate: bool,
    /// Maximum automatic rework rounds after an explicit `AuditVerdict::Fail` (capped at 5).
    pub max_audit_reworks: u32,
    /// Maximum automatic rework rounds from PR CI failures or owner reviews (capped at 3; 0 = off).
    pub max_pr_rework_rounds: u32,
    /// Rework count threshold after which implementation escalates to `implementer_escalated`.
    pub escalate_implementer_after_reworks: u32,
    pub plan_author: StageModelConfig,
    pub plan_challenger: StageModelConfig,
    pub implementer: StageModelConfig,
    pub implementer_escalated: StageModelConfig,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        PipelineConfig {
            plan_stage: false,
            plan_debate: false,
            require_plan_approval: false,
            max_plan_rounds: DEFAULT_MAX_PLAN_ROUNDS,
            allow_tier_s_skip_debate: false,
            max_audit_reworks: 3,
            max_pr_rework_rounds: crate::pipeline::DEFAULT_MAX_PR_REWORK_ROUNDS,
            escalate_implementer_after_reworks: DEFAULT_ESCALATE_IMPLEMENTER_AFTER_REWORKS,
            plan_author: StageModelConfig::default(),
            plan_challenger: StageModelConfig::default(),
            implementer: StageModelConfig::default(),
            implementer_escalated: StageModelConfig::default(),
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
    /// Optional host-side capture command loaded strictly from `base_sha`.
    #[serde(default)]
    pub capture_cmd: String,
    /// Optional globs scoping `proof.required` to matching diff paths.
    #[serde(default)]
    pub trigger_globs: Vec<String>,
    /// Capture script/asset globs enforced as `LockTier::Hard` from `base_sha`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capture_lock_globs: Vec<String>,
    /// Optional declarative proof flows loaded strictly from `base_sha`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flows: Vec<ProofFlow>,
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
            capture_cmd: String::new(),
            trigger_globs: Vec::new(),
            capture_lock_globs: Vec::new(),
            flows: Vec::new(),
        }
    }
}

pub use crate::commands_config::{CommandsConfig, ProjectConfig};

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
    /// renamed without a signed approval. Newly added test files
    /// ([`TEST_FILE_GLOBS`]) are exempt unless they also match a non-test
    /// locked glob; newly added build, runner, CI, and agent config files
    /// ([`RUNNER_CONFIG_GLOBS`], [`AGENT_CONFIG_GLOBS`], etc.) are locked.
    pub head_lock_globs: Vec<String>,
    pub store_compliance: StoreMode,
    pub min_android_target_sdk: u32,
    pub command_timeout_secs: u64,
    /// Output-silence timeout per gate command (seconds). Kills hung/GC-thrashing
    /// commands that emit no stdout/stderr for this long (`0` disables; time spent
    /// `SIGSTOP`-paused under memory pressure is not counted).
    pub idle_timeout_secs: u64,
    /// Skip `lint` and `test_scoped` in `--scoped` mode when every changed
    /// path is a safe documentation file (still runs hidden-file, static,
    /// head-lock, store-compliance, and proof checks).
    pub docs_fast_path: bool,
    /// Extra path globs excluded from `docs_fast_path`.
    pub docs_fast_path_exclude_globs: Vec<String>,
    /// Opt-in red->green & vacuity oracle (`off`, `warn`, `block`).
    #[serde(default, skip_serializing_if = "OracleMode::is_off")]
    pub red_green_oracle: OracleMode,
    /// Policy for `OracleOutcome::CompileRed` when `red_green_oracle = "block"`.
    #[serde(
        default,
        skip_serializing_if = "crate::oracle_config::OracleCompileRedPolicy::is_default"
    )]
    pub oracle_compile_red_policy: crate::oracle_config::OracleCompileRedPolicy,
}

// Re-export glob constants from the dedicated module so callers can import
// them without knowing the split, and to keep this file under 520 lines.
pub use crate::head_lock_globs::{
    AGENT_CONFIG_GLOBS, CI_AND_MANIFEST_GLOBS, RUNNER_CONFIG_GLOBS, TEST_FILE_GLOBS,
    default_head_lock_globs,
};

/// Google Play requirement for new apps and updates since 2026-08-31.
pub const DEFAULT_MIN_ANDROID_TARGET_SDK: u32 = 36;

impl Default for GateConfig {
    fn default() -> Self {
        GateConfig {
            base_ref: "auto".to_string(),
            head_lock_globs: default_head_lock_globs(),
            store_compliance: StoreMode::Auto,
            min_android_target_sdk: DEFAULT_MIN_ANDROID_TARGET_SDK,
            command_timeout_secs: 1800,
            idle_timeout_secs: 600,
            docs_fast_path: true,
            docs_fast_path_exclude_globs: Vec::new(),
            red_green_oracle: OracleMode::Off,
            oracle_compile_red_policy: crate::oracle_config::OracleCompileRedPolicy::Blocked,
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
    pub cost_guard: crate::pipeline::CostGuardConfig,
}

impl Default for UnblockerConfig {
    fn default() -> Self {
        UnblockerConfig {
            max_unblock_cycles: 10,
            max_subtask_depth: 1,
            max_token_budget: 2_000_000,
            max_wall_clock_secs: 10_800,
            global_freeze_consecutive_fails: 3,
            cost_guard: crate::pipeline::CostGuardConfig::default(),
        }
    }
}

/// Read through [`crate::builtin_defaults::RunConfigRaw`], so a file that
/// sets only `auto_merge` or only `land` gets the other to match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "crate::builtin_defaults::RunConfigRaw")]
pub struct RunConfig {
    /// `claude`, `codex`, `gemini`, `antigravity`, `opencode` or `custom`.
    pub executor: String,
    /// Command template for non-Claude executors; `{prompt}` is substituted.
    pub executor_cmd: Vec<String>,
    pub auto_merge: bool,
    #[serde(skip_serializing_if = "LandMode::is_default")]
    pub land: LandMode,
    /// Agent runs at the same time; `0` = automatic (accounts, CPUs, memory).
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
            land: LandMode::Pr,
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
    pub max_diff_bytes: Option<usize>,
    pub dual_review: Option<bool>,
    pub cross_adjudicate: Option<bool>,
    pub reviewers: Vec<ReviewerConfig>,
}

/// Default minimum average score (/100) required when `score_gate = true`.
pub const DEFAULT_MIN_AVERAGE_SCORE: u8 = 95;
/// Default minimum per-criterion score (/100) across G1..G7 when `score_gate = true`.
pub const DEFAULT_MIN_CRITERION_SCORE: u8 = 90;
/// Default max diff size (bytes) passed to cross-audit before fail-closed truncation.
pub const DEFAULT_CROSS_AUDIT_MAX_DIFF_BYTES: usize = 400_000;
/// Minimum allowed `cross_audit.max_diff_bytes`.
pub const MIN_CROSS_AUDIT_MAX_DIFF_BYTES: usize = 50_000;
/// Hard fail-closed ceiling for `cross_audit.max_diff_bytes`.
pub const MAX_CROSS_AUDIT_MAX_DIFF_BYTES: usize = 1_000_000;

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
    /// Fail closed instead of falling back to `same_account = true` when no other provider exists.
    pub require_different_provider: bool,
    pub audit_scope: AuditScope,
    pub score_gate: bool,
    pub min_average_score: u8,
    pub min_criterion_score: u8,
    pub max_diff_bytes: usize,
    pub dual_review: bool,
    pub cross_adjudicate: bool,
    pub reviewers: Vec<ReviewerConfig>,
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
            max_diff_bytes: DEFAULT_CROSS_AUDIT_MAX_DIFF_BYTES,
            dual_review: false,
            cross_adjudicate: true,
            reviewers: Vec::new(),
        }
    }
}
