//! Per-stage provider routing, provider-scoped account pools, Antigravity
//! process-wide serialization, deterministic tiering, and auto-escalation (§D.3).

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use aaf_domain::account::ProviderKind;
use aaf_domain::config::{AGENT_CONFIG_GLOBS, CrossAuditConfig, RepoConfig};
use aaf_domain::gate::ChangeKind;
use aaf_domain::pipeline::{PlanContract, TaskTier, canonical_provider, is_known_real_provider};
use aaf_domain::ports::{AgentRunner, PortError, PortResult};
use aaf_domain::task::{Priority, Task};
use globset::{GlobBuilder, GlobSetBuilder};

mod locked_pool;
pub use locked_pool::AntigravityLockedPool;

use super::adapters::provider_for;
use super::adopt_lane::AdoptEnv;
use super::audit_backend::{audit_cmd_is_claude, build_audit_command};
use super::ports::AccountPool;
use crate::gate::changes::Change;
use crate::gate::docs_fast;
use crate::plan_debate::schema::normalize_repo_path;

const HIGH_RISK_KEYWORDS: &[&str] = &[
    "sandbox",
    "hook",
    "auth",
    "security",
    "migration",
    "schema",
    "lock",
    "merge_queue",
];

/// Normalized executor/provider key isolating pools, cooldowns, and restarts.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProviderKey(pub String);

impl ProviderKey {
    pub fn from_executor(executor: &str) -> Self {
        let norm = aaf_domain::pipeline::normalize_executor_id(executor);
        Self(if norm.is_empty() {
            "default".to_string()
        } else {
            norm
        })
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn provider_kind(&self) -> Option<ProviderKind> {
        provider_for(&self.0)
    }
}

impl fmt::Display for ProviderKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Execution target for one pipeline stage.
pub struct StageTarget {
    pub executor: String,
    pub executor_cmd: String,
    pub model: String,
    pub effort: String,
    pub provider_key: ProviderKey,
    pub provider_kind: Option<ProviderKind>,
    pub hook_required: bool,
    pub runner: Arc<dyn AgentRunner>,
    pub accounts: Arc<dyn AccountPool>,
}

impl StageTarget {
    pub fn new(
        executor: impl Into<String>,
        executor_cmd: impl Into<String>,
        model: impl Into<String>,
        effort: impl Into<String>,
        runner: Arc<dyn AgentRunner>,
        accounts: Arc<dyn AccountPool>,
    ) -> Self {
        let (executor, executor_cmd) = (executor.into(), executor_cmd.into());
        let provider_key = ProviderKey::from_executor(&executor);
        let provider_kind = provider_key.provider_kind();
        let hook_required = stage_uses_claude(&executor, &executor_cmd);
        Self {
            executor,
            executor_cmd,
            model: model.into(),
            effort: effort.into(),
            provider_key,
            provider_kind,
            hook_required,
            runner,
            accounts,
        }
    }

    /// Same as [`Self::new`], but for code-writing stages (`default_implementer`,
    /// `implementer`, `implementer_escalated`) where non-Claude built-in hooked
    /// executors (`codex`, `gemini`, `antigravity`, `opencode`) also require
    /// their host `aaf-hook` registration.
    pub fn new_writer(
        executor: impl Into<String>,
        executor_cmd: impl Into<String>,
        model: impl Into<String>,
        effort: impl Into<String>,
        runner: Arc<dyn AgentRunner>,
        accounts: Arc<dyn AccountPool>,
    ) -> Self {
        let mut target = Self::new(executor, executor_cmd, model, effort, runner, accounts);
        target.hook_required = writer_requires_hook(&target.executor, &target.executor_cmd);
        target
    }
}

/// Central router mapping pipeline stages to [`StageTarget`] and [`AccountPool`].
pub struct StageRouter {
    pub default_implementer: StageTarget,
    pub implementer: Option<StageTarget>,
    pub implementer_escalated: Option<StageTarget>,
    pub plan_author: Option<StageTarget>,
    pub plan_challenger: Option<StageTarget>,
    pub reviewers: Vec<(String, StageTarget)>,
    /// The reviewers `aaf board adopt` asks, whatever `cross_audit.dual_review`
    /// says (see `adopt_reviewers`); their pools are registered like any stage's.
    pub adopt_reviewers: Vec<(String, StageTarget)>,
    /// What the adopt lane needs besides reviewers (`gh`, merge settings).
    /// `None` leaves `Adopting` tasks alone.
    pub adopt: Option<Arc<AdoptEnv>>,
    pub pools: HashMap<ProviderKey, Arc<dyn AccountPool>>,
}

impl StageRouter {
    pub fn new(default_implementer: StageTarget) -> Self {
        Self::from_targets(
            default_implementer,
            None,
            None,
            None,
            None,
            Vec::new(),
            HashMap::new(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_targets(
        default_implementer: StageTarget,
        implementer: Option<StageTarget>,
        implementer_escalated: Option<StageTarget>,
        plan_author: Option<StageTarget>,
        plan_challenger: Option<StageTarget>,
        reviewers: Vec<(String, StageTarget)>,
        mut pools: HashMap<ProviderKey, Arc<dyn AccountPool>>,
    ) -> Self {
        let all = [
            Some(&default_implementer),
            implementer.as_ref(),
            implementer_escalated.as_ref(),
            plan_author.as_ref(),
            plan_challenger.as_ref(),
        ];
        for target in all
            .into_iter()
            .flatten()
            .chain(reviewers.iter().map(|(_, t)| t))
        {
            pools
                .entry(target.provider_key.clone())
                .or_insert_with(|| Arc::clone(&target.accounts));
        }
        Self {
            default_implementer,
            implementer,
            implementer_escalated,
            plan_author,
            plan_challenger,
            reviewers,
            adopt_reviewers: Vec::new(),
            adopt: None,
            pools,
        }
    }

    /// Registers the adopt reviewers and their pools.
    pub fn with_adopt_reviewers(mut self, reviewers: Vec<(String, StageTarget)>) -> Self {
        for (_, target) in &reviewers {
            self.register_pool(target);
        }
        self.adopt_reviewers = reviewers;
        self
    }

    /// Turns the adopt lane on: `gh` and the merge settings it works with.
    pub fn with_adopt_env(mut self, env: AdoptEnv) -> Self {
        self.adopt = Some(Arc::new(env));
        self
    }

    pub fn adopt_reviewer(&self, id: &str) -> Option<&StageTarget> {
        self.adopt_reviewers
            .iter()
            .find(|(rid, _)| rid == id)
            .map(|(_, t)| t)
    }

    pub fn with_implementer(mut self, target: StageTarget) -> Self {
        self.register_pool(&target);
        self.implementer = Some(target);
        self
    }

    pub fn with_implementer_escalated(mut self, target: StageTarget) -> Self {
        self.register_pool(&target);
        self.implementer_escalated = Some(target);
        self
    }

    pub fn with_plan_author(mut self, target: StageTarget) -> Self {
        self.register_pool(&target);
        self.plan_author = Some(target);
        self
    }

    pub fn with_plan_challenger(mut self, target: StageTarget) -> Self {
        self.register_pool(&target);
        self.plan_challenger = Some(target);
        self
    }

    pub fn with_reviewers(mut self, reviewers: Vec<(String, StageTarget)>) -> Self {
        for (_, target) in &reviewers {
            self.register_pool(target);
        }
        self.reviewers = reviewers;
        self
    }

    pub fn with_pool(mut self, key: ProviderKey, pool: Arc<dyn AccountPool>) -> Self {
        self.pools.insert(key, pool);
        self
    }

    fn register_pool(&mut self, target: &StageTarget) {
        self.pools
            .entry(target.provider_key.clone())
            .or_insert_with(|| Arc::clone(&target.accounts));
    }

    pub fn pool_for(&self, key: &ProviderKey) -> Option<Arc<dyn AccountPool>> {
        self.pools.get(key).cloned()
    }

    pub fn default_provider_key(&self) -> ProviderKey {
        self.default_implementer.provider_key.clone()
    }

    pub fn resolve_plan_author(&self, cfg: &RepoConfig) -> PortResult<&StageTarget> {
        let author = self
            .plan_author
            .as_ref()
            .unwrap_or(&self.default_implementer);
        validate_plan_pair(
            author,
            self.plan_challenger.as_ref(),
            cfg.cross_audit.require_different_provider,
        )?;
        Ok(author)
    }

    pub fn resolve_plan_challenger(&self, cfg: &RepoConfig) -> PortResult<&StageTarget> {
        let challenger = self.plan_challenger.as_ref().ok_or_else(|| {
            PortError::Invalid(
                "pipeline.plan_challenger is not configured for plan debate".to_string(),
            )
        })?;
        let author = self
            .plan_author
            .as_ref()
            .unwrap_or(&self.default_implementer);
        validate_plan_pair(
            author,
            Some(challenger),
            cfg.cross_audit.require_different_provider,
        )?;
        Ok(challenger)
    }

    pub fn resolve_implementer(
        &self,
        task: &Task,
        plan: Option<&PlanContract>,
        cfg: &RepoConfig,
    ) -> (&StageTarget, TaskTier, bool) {
        let tier = classify_tier(task, plan, cfg);
        let reworks = rework_count(task);
        let threshold = cfg.pipeline.escalate_implementer_after_reworks;
        let use_escalated = tier == TaskTier::L || reworks >= threshold;
        let target = if use_escalated {
            self.implementer_escalated
                .as_ref()
                .or(self.implementer.as_ref())
                .unwrap_or(&self.default_implementer)
        } else {
            self.implementer
                .as_ref()
                .unwrap_or(&self.default_implementer)
        };
        let escalated =
            tier == TaskTier::M && reworks >= threshold && self.implementer_escalated.is_some();
        (target, tier, escalated)
    }

    pub fn reviewer(&self, id: &str) -> Option<&StageTarget> {
        self.reviewers
            .iter()
            .find(|(rid, _)| rid == id)
            .map(|(_, t)| t)
    }

    pub fn reviewers(&self) -> &[(String, StageTarget)] {
        &self.reviewers
    }
}

fn validate_plan_pair(
    author: &StageTarget,
    challenger: Option<&StageTarget>,
    require_different_provider: bool,
) -> PortResult<()> {
    if !require_different_provider {
        return Ok(());
    }
    let challenger = challenger.ok_or_else(|| {
        PortError::Invalid(
            "cross_audit.require_different_provider is true, but pipeline.plan_challenger is not configured"
                .to_string(),
        )
    })?;
    for (label, target) in [("plan_author", author), ("plan_challenger", challenger)] {
        if !target.executor_cmd.trim().is_empty() || !is_known_real_provider(&target.executor) {
            return Err(PortError::Invalid(format!(
                "cross_audit.require_different_provider is true, but {label} uses custom/unknown executor {:?}",
                target.executor
            )));
        }
    }
    let (a, c) = (
        canonical_provider(&author.executor),
        canonical_provider(&challenger.executor),
    );
    if a == c {
        return Err(PortError::Invalid(format!(
            "cross_audit.require_different_provider is true, but plan_author ({}) and plan_challenger ({}) share canonical provider {:?}",
            author.executor, challenger.executor, a
        )));
    }
    Ok(())
}

fn stage_uses_claude(executor: &str, executor_cmd: &str) -> bool {
    aaf_domain::pipeline::normalize_executor_id(executor) == "claude"
        || (!executor_cmd.trim().is_empty() && audit_cmd_is_claude(executor_cmd))
}

fn is_hooked_non_claude_executor(executor: &str) -> bool {
    matches!(
        aaf_domain::pipeline::normalize_executor_id(executor).as_str(),
        "codex" | "gemini" | "antigravity" | "opencode"
    )
}

/// True when a code-writing stage using `(executor, executor_cmd)` requires
/// its host `aaf-hook` registration (Claude always; built-in non-Claude hooked
/// hosts `codex`/`gemini`/`antigravity`/`opencode` when `executor_cmd` is empty).
pub fn writer_requires_hook(executor: &str, executor_cmd: &str) -> bool {
    stage_uses_claude(executor, executor_cmd)
        || (executor_cmd.trim().is_empty() && is_hooked_non_claude_executor(executor))
}

/// SEC-137: fail-closed check when joined `executor_cmd` may run `claude`.
fn cmd_may_run_claude(executor_cmd: &str) -> bool {
    let s = executor_cmd.trim();
    !s.is_empty()
        && (build_audit_command(&CrossAuditConfig {
            executor_cmd: s.into(),
            ..Default::default()
        })
        .is_err()
            || s.split_whitespace().any(|w| {
                Path::new(w.trim_matches(['\'', '"']))
                    .file_name()
                    .is_some_and(|n| n.eq_ignore_ascii_case("claude"))
            }))
}

/// Host executors for `aaf-hook` checks (SEC-137): always normalized `executor`, plus `"claude"`.
pub fn hook_host_executors(executor: &str, executor_cmd: &str) -> Vec<String> {
    let mut hosts = vec![aaf_domain::pipeline::normalize_executor_id(executor)];
    if hosts[0] != "claude"
        && (stage_uses_claude(executor, executor_cmd) || cmd_may_run_claude(executor_cmd))
    {
        hosts.push("claude".to_string());
    }
    hosts
}

/// Returns the set of lowercased non-Claude hooked executors required by any
/// configured code-writing stage (`run`, `pipeline.implementer`,
/// `pipeline.implementer_escalated`).
///
/// Must stay in sync with effective writer-stage resolution in
/// `crates/aaf-cli/src/cmd/run/stage_wiring.rs` (`build_stage_router`).
pub fn writer_hook_executors(cfg: &RepoConfig) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut record = |eff: &str, cmd: &str| {
        if cmd.trim().is_empty() && is_hooked_non_claude_executor(eff) {
            out.insert(aaf_domain::pipeline::normalize_executor_id(eff));
        }
    };
    let run_cmd = cfg.run.executor_cmd.join(" ");
    record(&cfg.run.executor, &run_cmd);
    for stage in [
        &cfg.pipeline.implementer,
        &cfg.pipeline.implementer_escalated,
    ] {
        if stage.is_empty() {
            continue;
        }
        let (exec, cmd) = (stage.executor.trim(), stage.executor_cmd.trim());
        let eff = match (exec, cmd) {
            ("", "") => cfg.run.executor.trim(),
            ("", _) => "custom",
            (v, _) => v,
        };
        record(eff, cmd);
    }
    out
}

/// True if ANY configured stage invokes Claude Code (R3-SEC-3).
pub fn any_stage_uses_claude(cfg: &RepoConfig) -> bool {
    let p = &cfg.pipeline;
    let run_cmd_claude = cfg
        .run
        .executor_cmd
        .first()
        .and_then(|a0| Path::new(a0).file_name())
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case("claude"));
    let stages = [
        &p.plan_author,
        &p.plan_challenger,
        &p.implementer,
        &p.implementer_escalated,
    ];
    aaf_domain::pipeline::normalize_executor_id(&cfg.run.executor) == "claude"
        || run_cmd_claude
        || stages
            .iter()
            .any(|s| stage_uses_claude(&s.executor, &s.executor_cmd))
        || stage_uses_claude(&cfg.cross_audit.executor, &cfg.cross_audit.executor_cmd)
        || cfg
            .cross_audit
            .reviewers
            .iter()
            .any(|r| stage_uses_claude(&r.executor, &r.executor_cmd))
}

/// Deterministic fail-closed Tier-S debate-skip check (§D.3.2).
pub fn is_tier_s_skip_debate(task: &Task, cfg: &RepoConfig) -> bool {
    if !cfg.pipeline.allow_tier_s_skip_debate
        || task.priority == Priority::P0
        || task.scope_paths.is_empty()
    {
        return false;
    }
    let Some(changes): Option<Vec<Change>> = task
        .scope_paths
        .iter()
        .map(|raw| {
            normalize_repo_path(raw).ok().map(|path| Change {
                kind: ChangeKind::Modified,
                path,
                old_path: None,
            })
        })
        .collect()
    else {
        return false;
    };
    docs_fast::is_docs_only(
        &changes,
        &cfg.gate.head_lock_globs,
        &cfg.gate.docs_fast_path_exclude_globs,
    )
    .unwrap_or(false)
}

/// Deterministically classifies `task` into `TaskTier::{S, M, L}` (§D.3.2).
pub fn classify_tier(task: &Task, plan: Option<&PlanContract>, cfg: &RepoConfig) -> TaskTier {
    if is_tier_s_skip_debate(task, cfg) && plan.is_none() {
        return TaskTier::S;
    }
    let plan_high_risk = plan.is_some_and(|p| {
        p.tier_assessment.trim().eq_ignore_ascii_case("L")
            || p.files_to_change.len() > 5
            || p.files_to_change
                .iter()
                .any(|f| is_high_risk_path(&f.path, &cfg.gate.head_lock_globs))
    });
    if task.priority == Priority::P0 || plan_high_risk {
        TaskTier::L
    } else {
        TaskTier::M
    }
}

/// True when `norm_path` matches a head-lock/agent-config glob, invalid path,
/// or high-risk security/concurrency keyword.
pub fn is_high_risk_path(norm_path: &str, head_lock_globs: &[String]) -> bool {
    let Ok(cleaned) = normalize_repo_path(norm_path) else {
        return true;
    };
    let mut builder = GlobSetBuilder::new();
    for g in head_lock_globs
        .iter()
        .map(String::as_str)
        .chain(AGENT_CONFIG_GLOBS.iter().copied())
    {
        let Ok(compiled) = GlobBuilder::new(g).literal_separator(true).build() else {
            return true;
        };
        builder.add(compiled);
    }
    let Ok(set) = builder.build() else {
        return true;
    };
    if set.is_match(&cleaned) {
        return true;
    }
    let lower = cleaned.to_ascii_lowercase();
    HIGH_RISK_KEYWORDS.iter().any(|kw| lower.contains(kw))
}

/// Counts reworks since the latest `"rework:"` or `"unblocked:"` note,
/// counting ONLY `"audit_rework:"` and `"unblock[T3]"` (excluding `T2`, R3-CONC-7).
pub fn rework_count(task: &Task) -> u32 {
    let notes = &task.context_notes;
    let start = notes
        .iter()
        .rposition(|n| n.starts_with("rework:") || n.starts_with("unblocked:"))
        .map_or(0, |idx| idx + 1);
    let count = notes[start..]
        .iter()
        .filter(|n| n.starts_with("audit_rework:") || n.starts_with("unblock[T3]"))
        .count();
    u32::try_from(count).unwrap_or(u32::MAX)
}
