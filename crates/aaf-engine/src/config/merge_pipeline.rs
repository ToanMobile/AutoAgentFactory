//! Tier-1 -> Tier-2 merge and Tier-3 overlay rules for `[pipeline]`,
//! Dual-Review `[cross_audit]`, and `[unblocker]` (docs/PLAN_DUAL_FRONTIER.md §C.2).

use std::path::Path;

use aaf_domain::config::{
    CrossAuditConfig, GlobalCrossAuditConfig, LandMode, MAX_AUDIT_REWORKS_CAP, PipelineConfig,
    RepoConfig, UnblockerConfig,
};
use aaf_domain::pipeline::{
    CostGuardConfig, DEFAULT_SPIKE_FACTOR, GlobalCostGuardConfig, GlobalPipelineConfig,
    GlobalUnblockerConfig, MAX_PLAN_ROUNDS_CAP, MAX_PR_REWORK_ROUNDS_CAP, StageModelConfig,
    distinct_real_reviewer_providers, validate_reviewers,
};
use serde::Deserialize;

/// Tracks which Tier-2 `.autofactory/config.toml` keys were explicitly present
/// in the raw TOML so Tier-1 `Option<...>` defaults distinguish "omitted" from
/// "explicitly set to default".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RepoKeyPresence {
    pub allow_tier_s_skip_debate: bool,
    pub max_plan_rounds: bool,
    pub max_audit_reworks: bool,
    pub max_pr_rework_rounds: bool,
    pub escalate_implementer_after_reworks: bool,
    pub cross_adjudicate: bool,
    pub max_token_budget: bool,
    pub max_wall_clock_secs: bool,
    pub max_unblock_cycles: bool,
    pub cost_guard_spike_factor: bool,
    pub run_auto_merge: bool,
    pub run_land: bool,
}

impl RepoKeyPresence {
    pub(crate) fn all_unspecified() -> Self {
        Self::default()
    }

    /// Fail-closed mask for `tighten_trusted_base`: prevents Tier-1 `Some(true)`
    /// defaults from weakening an explicit `false` in the trusted base.
    pub(crate) fn all_explicit() -> Self {
        Self {
            allow_tier_s_skip_debate: true,
            max_plan_rounds: true,
            max_audit_reworks: true,
            max_pr_rework_rounds: true,
            escalate_implementer_after_reworks: true,
            cross_adjudicate: true,
            max_token_budget: true,
            max_wall_clock_secs: true,
            max_unblock_cycles: true,
            cost_guard_spike_factor: false,
            run_auto_merge: false,
            run_land: false,
        }
    }

    pub(crate) fn from_table(table: &toml::Table) -> Self {
        Self {
            allow_tier_s_skip_debate: has_key(table, "pipeline", "allow_tier_s_skip_debate"),
            max_plan_rounds: has_key(table, "pipeline", "max_plan_rounds"),
            max_audit_reworks: has_key(table, "pipeline", "max_audit_reworks"),
            max_pr_rework_rounds: has_key(table, "pipeline", "max_pr_rework_rounds"),
            escalate_implementer_after_reworks: has_key(
                table,
                "pipeline",
                "escalate_implementer_after_reworks",
            ),
            cross_adjudicate: has_key(table, "cross_audit", "cross_adjudicate"),
            max_token_budget: has_key(table, "unblocker", "max_token_budget"),
            max_wall_clock_secs: has_key(table, "unblocker", "max_wall_clock_secs"),
            max_unblock_cycles: has_key(table, "unblocker", "max_unblock_cycles"),
            cost_guard_spike_factor: has_nested_key(
                table,
                "unblocker",
                "cost_guard",
                "spike_factor",
            ),
            run_auto_merge: has_key(table, "run", "auto_merge"),
            run_land: has_key(table, "run", "land"),
        }
    }
}

/// Validates Tier-2 `[run]` `auto_merge` vs `land` consistency and applies
/// Tier-1 `run.auto_merge_repos` opt-in when neither Tier-2 key was specified.
pub(crate) fn reconcile_land_mode(
    auto_merge_repos: &[String],
    repo_root: Option<&Path>,
    repo: &mut RepoConfig,
    presence: &RepoKeyPresence,
) -> Result<(), String> {
    if presence.run_auto_merge && presence.run_land {
        if repo.run.auto_merge != repo.run.land.auto_lands() {
            return Err(format!(
                "[run] conflict: auto_merge = {} contradicts land = {:?}",
                repo.run.auto_merge,
                repo.run.land.as_str()
            ));
        }
    } else if presence.run_auto_merge {
        if repo.run.auto_merge {
            repo.run.land = LandMode::Auto;
        }
    } else if presence.run_land {
        repo.run.auto_merge = repo.run.land.auto_lands();
    } else if let Some(root) = repo_root
        && matches_auto_merge_repo(auto_merge_repos, root)
    {
        repo.run.auto_merge = true;
        repo.run.land = LandMode::Auto;
    }
    Ok(())
}

fn matches_auto_merge_repo(allowlist: &[String], repo_root: &Path) -> bool {
    if allowlist.is_empty() {
        return false;
    }
    let canon_root = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    allowlist.iter().any(|entry| {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            return false;
        }
        let path = Path::new(trimmed);
        let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        canon == canon_root
    })
}

fn has_key(table: &toml::Table, section: &str, key: &str) -> bool {
    table
        .get(section)
        .and_then(toml::Value::as_table)
        .is_some_and(|sub| sub.contains_key(key))
}

fn has_nested_key(table: &toml::Table, section: &str, subsection: &str, key: &str) -> bool {
    table
        .get(section)
        .and_then(toml::Value::as_table)
        .and_then(|sub| sub.get(subsection))
        .and_then(toml::Value::as_table)
        .is_some_and(|sub| sub.contains_key(key))
}

/// Parses raw Tier-2 TOML text to record which merge-sensitive keys were explicitly set.
pub(crate) fn parse_repo_key_presence(toml_text: &str) -> RepoKeyPresence {
    toml_text.parse::<toml::Table>().map_or_else(
        |_| RepoKeyPresence::all_unspecified(),
        |t| RepoKeyPresence::from_table(&t),
    )
}

/// Tier 3 partial `[pipeline]` section:
/// - `plan_stage`, `plan_debate`, `require_plan_approval`: tighten-only (`false -> true`).
/// - `allow_tier_s_skip_debate`: tighten-only (`true -> false`).
/// - Numeric caps & stage model overrides are always ignored with a warning.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct PipelineOverlay {
    pub plan_stage: Option<bool>,
    pub plan_debate: Option<bool>,
    pub require_plan_approval: Option<bool>,
    pub allow_tier_s_skip_debate: Option<bool>,
    pub max_plan_rounds: Option<u32>,
    pub max_audit_reworks: Option<u32>,
    pub max_pr_rework_rounds: Option<u32>,
    pub escalate_implementer_after_reworks: Option<u32>,
    pub plan_author: Option<StageModelConfig>,
    pub plan_challenger: Option<StageModelConfig>,
    pub implementer: Option<StageModelConfig>,
    pub implementer_escalated: Option<StageModelConfig>,
}

fn clamp_range(value: &mut u32, min: u32, max: u32, label: &str, warnings: &mut Vec<String>) {
    if *value < min {
        warnings.push(format!("{label} = {} clamped to {min}", *value));
        *value = min;
    } else if *value > max {
        warnings.push(format!("{label} = {} clamped to {max}", *value));
        *value = max;
    }
}

fn inherit_stage(global: &Option<StageModelConfig>, repo: &mut StageModelConfig) {
    if repo.is_empty()
        && let Some(g) = global
        && !g.is_empty()
    {
        *repo = g.clone();
    }
}

/// Merges Tier-1 `[pipeline]` floors/defaults into Tier-2 `[pipeline]` and
/// enforces hard numeric caps (`1..=5` for debate/escalation, `0..=5` for
/// `max_audit_reworks`).
pub(crate) fn merge_pipeline(
    global: &GlobalPipelineConfig,
    repo: &mut PipelineConfig,
    presence: &RepoKeyPresence,
    source: &str,
    warnings: &mut Vec<String>,
) {
    if global.plan_stage == Some(true) {
        repo.plan_stage = true;
    }
    if global.plan_debate == Some(true) {
        repo.plan_debate = true;
    }
    if repo.plan_debate {
        repo.plan_stage = true;
    }
    if global.require_plan_approval == Some(true) {
        repo.require_plan_approval = true;
    }
    match global.allow_tier_s_skip_debate {
        Some(false) => {
            if repo.allow_tier_s_skip_debate {
                warnings.push(format!(
                    "{source}: pipeline.allow_tier_s_skip_debate = true overridden by global false"
                ));
            }
            repo.allow_tier_s_skip_debate = false;
        }
        Some(true) if !presence.allow_tier_s_skip_debate => {
            repo.allow_tier_s_skip_debate = true;
        }
        Some(true) | None => {}
    }
    if !presence.max_plan_rounds
        && let Some(g) = global.max_plan_rounds
    {
        repo.max_plan_rounds = g;
    }
    clamp_range(
        &mut repo.max_plan_rounds,
        1,
        MAX_PLAN_ROUNDS_CAP,
        &format!("{source}: pipeline.max_plan_rounds"),
        warnings,
    );
    if !presence.max_audit_reworks
        && let Some(g) = global.max_audit_reworks
    {
        repo.max_audit_reworks = g;
    }
    if repo.max_audit_reworks > MAX_AUDIT_REWORKS_CAP {
        warnings.push(format!(
            "{source}: pipeline.max_audit_reworks = {} clamped to {MAX_AUDIT_REWORKS_CAP}",
            repo.max_audit_reworks
        ));
        repo.max_audit_reworks = MAX_AUDIT_REWORKS_CAP;
    }
    if !presence.max_pr_rework_rounds
        && let Some(g) = global.max_pr_rework_rounds
    {
        repo.max_pr_rework_rounds = g;
    }
    if repo.max_pr_rework_rounds > MAX_PR_REWORK_ROUNDS_CAP {
        warnings.push(format!(
            "{source}: pipeline.max_pr_rework_rounds = {} clamped to {MAX_PR_REWORK_ROUNDS_CAP}",
            repo.max_pr_rework_rounds
        ));
        repo.max_pr_rework_rounds = MAX_PR_REWORK_ROUNDS_CAP;
    }
    if !presence.escalate_implementer_after_reworks
        && let Some(g) = global.escalate_implementer_after_reworks
    {
        repo.escalate_implementer_after_reworks = g;
    }
    clamp_range(
        &mut repo.escalate_implementer_after_reworks,
        1,
        MAX_AUDIT_REWORKS_CAP,
        &format!("{source}: pipeline.escalate_implementer_after_reworks"),
        warnings,
    );
    inherit_stage(&global.plan_author, &mut repo.plan_author);
    inherit_stage(&global.plan_challenger, &mut repo.plan_challenger);
    inherit_stage(&global.implementer, &mut repo.implementer);
    inherit_stage(
        &global.implementer_escalated,
        &mut repo.implementer_escalated,
    );
}

/// Validates reviewer IDs in both Tier 1 and Tier 2 and merges Dual-Review
/// `[cross_audit]` settings monotonically.
///
/// - `dual_review`: `T1 || T2`.
/// - `cross_adjudicate`: `false` is stricter than `true` (R3-SEC-4).
/// - `reviewers`: Tier 2 wins unless `T1.dual_review == Some(true)` and Tier 2
///   has `< 2` distinct real canonical providers while Tier 1 has `>= 2`
///   (R3-SEC-5).
pub(crate) fn merge_cross_audit_dual(
    global: &GlobalCrossAuditConfig,
    repo: &mut CrossAuditConfig,
    presence: &RepoKeyPresence,
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    validate_reviewers(&global.reviewers)
        .map_err(|err| format!("global cross_audit.reviewers: {err}"))?;
    validate_reviewers(&repo.reviewers).map_err(|err| format!("cross_audit.reviewers: {err}"))?;

    if global.dual_review == Some(true) {
        repo.dual_review = true;
    }
    match global.cross_adjudicate {
        Some(false) => {
            if presence.cross_adjudicate && repo.cross_adjudicate {
                warnings.push(
                    "cross_audit.cross_adjudicate = true overridden by global false".to_string(),
                );
            }
            repo.cross_adjudicate = false;
        }
        Some(true) if !presence.cross_adjudicate => {
            repo.cross_adjudicate = true;
        }
        Some(true) | None => {}
    }
    if repo.reviewers.is_empty() {
        repo.reviewers = global.reviewers.clone();
    } else if global.dual_review == Some(true) {
        let repo_distinct = distinct_real_reviewer_providers(&repo.reviewers);
        let global_distinct = distinct_real_reviewer_providers(&global.reviewers);
        if repo_distinct < 2 && global_distinct >= 2 {
            warnings.push(format!(
                "cross_audit.reviewers ignored: only {repo_distinct} distinct real canonical \
                 provider(s) while global dual_review = true (kept global reviewers)"
            ));
            repo.reviewers = global.reviewers.clone();
        }
    }
    Ok(())
}

fn merge_floor_u64(global: Option<u64>, repo: &mut u64, present: bool, floor_active: bool) {
    if let Some(g) = global {
        if !present {
            *repo = g;
        } else if floor_active {
            *repo = (*repo).max(g);
        }
    }
}

fn tighten_cap_u64(global: Option<u64>, repo: u64) -> u64 {
    match global {
        Some(g) if g > 0 => {
            if repo == 0 {
                g
            } else {
                g.min(repo)
            }
        }
        None | Some(_) => repo,
    }
}

fn tighten_cap_u32(global: Option<u32>, repo: u32) -> u32 {
    match global {
        Some(g) if g > 0 => {
            if repo == 0 {
                g
            } else {
                g.min(repo)
            }
        }
        None | Some(_) => repo,
    }
}

/// Merges Tier-1 `[unblocker.cost_guard]` into Tier-2 with tighten-only (`min`)
/// semantics: a repository may enable or lower a positive Tier-1 cap/factor,
/// never loosen or disable it. When Tier 1 sets a positive `daily_token_cap`
/// without `spike_factor`, the default spike guard (`DEFAULT_SPIKE_FACTOR`)
/// applies regardless of repo presence; only Tier 1 `spike_factor = 0` can turn
/// it off. A repo may disable spike detection only for a cap it enables itself.
fn merge_cost_guard(
    global: &GlobalCostGuardConfig,
    repo: &mut CostGuardConfig,
    presence: &RepoKeyPresence,
) {
    repo.daily_token_cap = tighten_cap_u64(global.daily_token_cap, repo.daily_token_cap);
    let effective_global_spike = match global.spike_factor {
        Some(s) => Some(s),
        None if global.daily_token_cap.is_some_and(|c| c > 0) => Some(DEFAULT_SPIKE_FACTOR),
        None => None,
    };
    repo.spike_factor = tighten_cap_u32(effective_global_spike, repo.spike_factor);
    if repo.daily_token_cap > 0
        && repo.spike_factor == 0
        && global.spike_factor.is_none()
        && !presence.cost_guard_spike_factor
    {
        repo.spike_factor = DEFAULT_SPIKE_FACTOR;
    }
}

/// Merges Tier-1 `[unblocker]` defaults/floors into Tier-2 `[unblocker]`.
/// When `dual_or_debate_active` is `true`, Tier 1 acts as a minimum floor
/// (`repo.max(global)`) so `aaf init` defaults do not starve dual-frontier runs.
pub(crate) fn merge_unblocker(
    global: &GlobalUnblockerConfig,
    repo: &mut UnblockerConfig,
    presence: &RepoKeyPresence,
    dual_or_debate_active: bool,
) {
    merge_floor_u64(
        global.max_token_budget,
        &mut repo.max_token_budget,
        presence.max_token_budget,
        dual_or_debate_active,
    );
    merge_floor_u64(
        global.max_wall_clock_secs,
        &mut repo.max_wall_clock_secs,
        presence.max_wall_clock_secs,
        dual_or_debate_active,
    );
    if let Some(g) = global.max_unblock_cycles {
        if !presence.max_unblock_cycles {
            repo.max_unblock_cycles = g;
        } else if dual_or_debate_active {
            repo.max_unblock_cycles = repo.max_unblock_cycles.max(g);
        }
    }
    merge_cost_guard(&global.cost_guard, &mut repo.cost_guard, presence);
}

fn tighten_true(
    current: &mut bool,
    overlay: Option<bool>,
    source: &str,
    key: &str,
    warnings: &mut Vec<String>,
) {
    match overlay {
        Some(true) => *current = true,
        Some(false) if *current => warnings.push(format!(
            "{source}: {key} = false ignored: it may only be raised (kept true)"
        )),
        Some(false) | None => {}
    }
}

/// Applies Tier-3 `[pipeline]` overlay (tighten-only booleans; numeric/stage ignored).
pub(crate) fn apply_pipeline_overlay(
    current: &mut PipelineConfig,
    overlay: PipelineOverlay,
    source: &str,
    warnings: &mut Vec<String>,
) {
    const TRUSTED_ONLY: &str = "only the global config or .autofactory/config.toml may set it";
    tighten_true(
        &mut current.plan_debate,
        overlay.plan_debate,
        source,
        "pipeline.plan_debate",
        warnings,
    );
    if current.plan_debate {
        current.plan_stage = true;
    }
    tighten_true(
        &mut current.plan_stage,
        overlay.plan_stage,
        source,
        "pipeline.plan_stage",
        warnings,
    );
    tighten_true(
        &mut current.require_plan_approval,
        overlay.require_plan_approval,
        source,
        "pipeline.require_plan_approval",
        warnings,
    );
    match overlay.allow_tier_s_skip_debate {
        Some(false) => current.allow_tier_s_skip_debate = false,
        Some(true) if !current.allow_tier_s_skip_debate => warnings.push(format!(
            "{source}: pipeline.allow_tier_s_skip_debate = true ignored: it may only be turned off (kept false)"
        )),
        Some(true) | None => {}
    }
    for (field, attempted, kept) in [
        (
            "max_plan_rounds",
            overlay.max_plan_rounds,
            current.max_plan_rounds,
        ),
        (
            "max_audit_reworks",
            overlay.max_audit_reworks,
            current.max_audit_reworks,
        ),
        (
            "max_pr_rework_rounds",
            overlay.max_pr_rework_rounds,
            current.max_pr_rework_rounds,
        ),
        (
            "escalate_implementer_after_reworks",
            overlay.escalate_implementer_after_reworks,
            current.escalate_implementer_after_reworks,
        ),
    ] {
        if let Some(val) = attempted {
            warnings.push(format!(
                "{source}: pipeline.{field} = {val} ignored: {TRUSTED_ONLY} (kept {kept})"
            ));
        }
    }
    for (field, stage) in [
        ("plan_author", overlay.plan_author),
        ("plan_challenger", overlay.plan_challenger),
        ("implementer", overlay.implementer),
        ("implementer_escalated", overlay.implementer_escalated),
    ] {
        if stage.is_some() {
            warnings.push(format!(
                "{source}: pipeline.{field} ignored: {TRUSTED_ONLY}"
            ));
        }
    }
}
