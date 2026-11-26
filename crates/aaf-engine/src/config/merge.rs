//! Overrides from tier 3 and above, and the rules for applying them.

use aaf_domain::config::{
    AuditScope, CrossAuditConfig, GlobalCrossAuditConfig, LandMode, OracleMode, RepoConfig,
    StoreMode,
};
use aaf_domain::oracle_config::OracleCompileRedPolicy;
use aaf_domain::pipeline::ReviewerConfig;
use serde::Deserialize;

/// A partial configuration from the local file, the environment or the
/// command line. `None` means "not given", which is different from "given
/// with the default value".
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConfigOverlay {
    pub project: ProjectOverlay,
    pub gate: GateOverlay,
    pub run: RunOverlay,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProjectOverlay {
    pub name: Option<String>,
    pub profile: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GateOverlay {
    /// Accepted so that it can be reported; never applied.
    pub base_ref: Option<String>,
    /// Added to the tier 2 list.
    pub head_lock_globs: Option<Vec<String>>,
    pub store_compliance: Option<StoreMode>,
    pub min_android_target_sdk: Option<u32>,
    pub command_timeout_secs: Option<u64>,
    pub idle_timeout_secs: Option<u64>,
    pub docs_fast_path: Option<bool>,
    pub docs_fast_path_exclude_globs: Option<Vec<String>>,
    pub red_green_oracle: Option<OracleMode>,
    pub oracle_compile_red_policy: Option<OracleCompileRedPolicy>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunOverlay {
    pub executor: Option<String>,
    pub executor_cmd: Option<Vec<String>>,
    pub auto_merge: Option<bool>,
    pub land: Option<LandMode>,
    pub max_parallel: Option<u32>,
    pub run_timeout_secs: Option<u64>,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Accepted so that it can be reported; never applied.
    pub env_pass: Option<Vec<String>>,
    /// Queue brake override: may only tighten downward to a positive lower cap.
    pub max_open_review: Option<u32>,
}

/// Tier 3 partial `[cross_audit]` section: tighten-only on
/// `require_different_provider`, `audit_scope`, `score_gate`,
/// `min_average_score`, `min_criterion_score`, `dual_review` (`false -> true`),
/// and `cross_adjudicate` (`true -> false`). Provider fields and `reviewers`
/// are accepted solely so they can be reported as ignored with a warning.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct CrossAuditOverlay {
    pub executor: Option<String>,
    pub executor_cmd: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub require_different_provider: Option<bool>,
    pub audit_scope: Option<AuditScope>,
    pub score_gate: Option<bool>,
    pub min_average_score: Option<u8>,
    pub min_criterion_score: Option<u8>,
    pub max_diff_bytes: Option<usize>,
    pub dual_review: Option<bool>,
    pub cross_adjudicate: Option<bool>,
    pub reviewers: Option<Vec<ReviewerConfig>>,
}

fn clamp_score(value: u8, label: &str, warnings: &mut Vec<String>) -> u8 {
    if value > 100 {
        warnings.push(format!("{label} = {value} clamped to 100"));
        100
    } else {
        value
    }
}

/// Merges tier 1 (`global`) defaults/floors into tier 2 (`repo`): non-empty
/// tier 2 provider fields win, while unspecified tier-1 `Option<...>` values
/// never overwrite explicit tier-2 values. Policy fields tighten monotonically
/// from tier 1 into tier 2, and score thresholds are clamped to `0..=100`.
pub(crate) fn merge_cross_audit(
    global: &GlobalCrossAuditConfig,
    repo: &mut CrossAuditConfig,
    warnings: &mut Vec<String>,
) {
    let repo_exec = repo.executor.trim();
    let global_exec = global.executor.trim();
    // `auto` (the built-in default) asks aaf to pick an auditor; the
    // operator's machine-wide choice is that pick.
    let repo_exec = if repo_exec.eq_ignore_ascii_case("auto")
        && !global_exec.is_empty()
        && !global_exec.eq_ignore_ascii_case("auto")
    {
        ""
    } else {
        repo_exec
    };
    let inherit_provider_fields =
        repo_exec.is_empty() || repo_exec.eq_ignore_ascii_case(global_exec);
    if repo_exec.is_empty() {
        repo.executor = global.executor.clone();
    }
    if inherit_provider_fields {
        if repo.executor_cmd.trim().is_empty() {
            repo.executor_cmd = global.executor_cmd.clone();
        }
        if repo.model.trim().is_empty() {
            repo.model = global.model.clone();
        }
        if repo.effort.trim().is_empty() {
            repo.effort = global.effort.clone();
        }
    }
    repo.min_average_score = clamp_score(
        repo.min_average_score,
        "cross_audit.min_average_score",
        warnings,
    );
    repo.min_criterion_score = clamp_score(
        repo.min_criterion_score,
        "cross_audit.min_criterion_score",
        warnings,
    );
    if global.require_different_provider == Some(true) {
        repo.require_different_provider = true;
    }
    if let Some(g) = global.audit_scope {
        repo.audit_scope = repo.audit_scope.max(g);
    }
    if global.score_gate == Some(true) {
        repo.score_gate = true;
    }
    if let Some(g) = global.min_average_score {
        let g = clamp_score(g, "global cross_audit.min_average_score", warnings);
        repo.min_average_score = repo.min_average_score.max(g);
    }
    if let Some(g) = global.min_criterion_score {
        let g = clamp_score(g, "global cross_audit.min_criterion_score", warnings);
        repo.min_criterion_score = repo.min_criterion_score.max(g);
    }
    let clamp_diff = |v: usize, label: &str, w: &mut Vec<String>| -> usize {
        let lo = aaf_domain::config::MIN_CROSS_AUDIT_MAX_DIFF_BYTES;
        let hi = aaf_domain::config::MAX_CROSS_AUDIT_MAX_DIFF_BYTES;
        let clamped = v.clamp(lo, hi);
        if clamped != v {
            w.push(format!("{label} = {v} clamped to {clamped} ({lo}..={hi})"));
        }
        clamped
    };
    if let Some(g) = global.max_diff_bytes
        && repo.max_diff_bytes == aaf_domain::config::DEFAULT_CROSS_AUDIT_MAX_DIFF_BYTES
    {
        repo.max_diff_bytes = clamp_diff(g, "global cross_audit.max_diff_bytes", warnings);
    } else {
        repo.max_diff_bytes =
            clamp_diff(repo.max_diff_bytes, "cross_audit.max_diff_bytes", warnings);
    }
}

/// Applies tier 3 `[cross_audit]` overlay: tighten-only on policy fields
/// (`require_different_provider`, `audit_scope`, `score_gate`,
/// `min_average_score`, `min_criterion_score`); executor/cmd/model/effort
/// changes are ignored with a warning.
pub(crate) fn apply_cross_audit_overlay(
    current: &mut CrossAuditConfig,
    overlay: CrossAuditOverlay,
    source: &str,
    warnings: &mut Vec<String>,
) {
    const TRUSTED_ONLY: &str = "only the global config or .autofactory/config.toml may set it";
    if let Some(executor) = overlay.executor
        && executor != current.executor
    {
        warnings.push(format!(
            "{source}: cross_audit.executor = {executor:?} ignored: {TRUSTED_ONLY} (kept {:?})",
            current.executor
        ));
    }
    if let Some(executor_cmd) = overlay.executor_cmd
        && executor_cmd != current.executor_cmd
    {
        warnings.push(format!(
            "{source}: cross_audit.executor_cmd ignored: {TRUSTED_ONLY}"
        ));
    }
    if let Some(model) = overlay.model
        && model != current.model
    {
        warnings.push(format!(
            "{source}: cross_audit.model = {model:?} ignored: {TRUSTED_ONLY} (kept {:?})",
            current.model
        ));
    }
    if let Some(effort) = overlay.effort
        && effort != current.effort
    {
        warnings.push(format!(
            "{source}: cross_audit.effort = {effort:?} ignored: {TRUSTED_ONLY} (kept {:?})",
            current.effort
        ));
    }
    match overlay.require_different_provider {
        Some(true) => current.require_different_provider = true,
        Some(false) if current.require_different_provider => {
            warnings.push(format!(
                "{source}: cross_audit.require_different_provider = false ignored: it may only be raised (kept true)"
            ));
        }
        Some(false) | None => {}
    }
    if let Some(scope) = overlay.audit_scope {
        if scope > current.audit_scope {
            current.audit_scope = scope;
        } else if scope < current.audit_scope {
            warnings.push(format!(
                "{source}: cross_audit.audit_scope = {} ignored: it may only be raised (kept {})",
                scope.as_str(),
                current.audit_scope.as_str()
            ));
        }
    }
    match overlay.score_gate {
        Some(true) => current.score_gate = true,
        Some(false) if current.score_gate => {
            warnings.push(format!(
                "{source}: cross_audit.score_gate = false ignored: it may only be raised (kept true)"
            ));
        }
        Some(false) | None => {}
    }
    if let Some(raw) = overlay.min_average_score {
        let score = clamp_score(
            raw,
            &format!("{source}: cross_audit.min_average_score"),
            warnings,
        );
        if score > current.min_average_score {
            current.min_average_score = score;
        } else if score < current.min_average_score {
            warnings.push(format!(
                "{source}: cross_audit.min_average_score = {score} ignored: it may only be raised (kept {})",
                current.min_average_score
            ));
        }
    }
    if let Some(raw) = overlay.min_criterion_score {
        let score = clamp_score(
            raw,
            &format!("{source}: cross_audit.min_criterion_score"),
            warnings,
        );
        if score > current.min_criterion_score {
            current.min_criterion_score = score;
        } else if score < current.min_criterion_score {
            warnings.push(format!(
                "{source}: cross_audit.min_criterion_score = {score} ignored: it may only be raised (kept {})",
                current.min_criterion_score
            ));
        }
    }
    match overlay.dual_review {
        Some(true) => current.dual_review = true,
        Some(false) if current.dual_review => {
            warnings.push(format!(
                "{source}: cross_audit.dual_review = false ignored: it may only be raised (kept true)"
            ));
        }
        Some(false) | None => {}
    }
    match overlay.cross_adjudicate {
        Some(false) => current.cross_adjudicate = false,
        Some(true) if !current.cross_adjudicate => {
            warnings.push(format!(
                "{source}: cross_audit.cross_adjudicate = true ignored: false is stricter (kept false)"
            ));
        }
        Some(true) | None => {}
    }
    if overlay.max_diff_bytes.is_some() {
        warnings.push(format!(
            "{source}: cross_audit.max_diff_bytes ignored: {TRUSTED_ONLY}"
        ));
    }
    if overlay.reviewers.is_some() {
        warnings.push(format!(
            "{source}: cross_audit.reviewers ignored: {TRUSTED_ONLY}"
        ));
    }
}

/// Merges tier 1 `[run].max_open_review` into tier 2. A positive tier-1 cap
/// cannot be loosened or disabled (`0`) by tier 2.
pub(crate) fn merge_max_open_review(global: Option<u32>, repo: &mut u32) {
    if let Some(cap) = global
        && cap > 0
    {
        *repo = if *repo > 0 { cap.min(*repo) } else { cap };
    }
}

fn mode_name(mode: StoreMode) -> &'static str {
    match mode {
        StoreMode::Off => "off",
        StoreMode::Auto => "auto",
        StoreMode::Always => "always",
    }
}

/// Where an overlay comes from, which decides what it may switch on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OverlayOrigin {
    /// The uncommitted local file or the environment: both can be set by an
    /// agent working in the checkout without leaving a diff.
    Ambient,
    /// A flag the operator typed for this one invocation.
    CommandLine,
}

/// Applies one overlay on top of `repo`. Nothing in it can weaken the gate:
/// `gate.*` (except the timeout) can only become stricter; `project.profile`,
/// `run.executor` and `run.executor_cmd` belong to the committed file and are
/// never taken from an overlay; `run.auto_merge` can always be switched off
/// but is switched on only by a command-line flag; `run.env_pass` decides
/// which variables of the operator's environment an agent can read and
/// belongs to the committed file as well. `project.name`,
/// `run.max_parallel`, `run.run_timeout_secs`, `run.model`, `run.effort` and
/// `gate.command_timeout_secs` are taken as given. Every dropped value leaves
/// a warning that names `source`.
pub(crate) fn apply_overlay(
    repo: &mut RepoConfig,
    overlay: ConfigOverlay,
    source: &str,
    origin: OverlayOrigin,
    warnings: &mut Vec<String>,
) {
    const TIER_TWO_ONLY: &str = "only .autofactory/config.toml may set it";
    let ConfigOverlay { project, gate, run } = overlay;

    if let Some(name) = project.name {
        repo.project.name = name;
    }
    if let Some(profile) = project.profile
        && profile != repo.project.profile
    {
        warnings.push(format!(
            "{source}: project.profile = {profile:?} ignored: {TIER_TWO_ONLY} (kept {:?})",
            repo.project.profile
        ));
    }

    // The executor and its command template decide which program runs with
    // the account's credentials.
    if let Some(executor) = run.executor
        && executor != repo.run.executor
    {
        warnings.push(format!(
            "{source}: run.executor = {executor:?} ignored: {TIER_TWO_ONLY} (kept {:?})",
            repo.run.executor
        ));
    }
    if let Some(executor_cmd) = run.executor_cmd
        && executor_cmd != repo.run.executor_cmd
    {
        warnings.push(format!(
            "{source}: run.executor_cmd ignored: {TIER_TWO_ONLY}"
        ));
    }
    // Reconcile Tier-2 legacy `auto_merge = true` with default `land = Pr`
    // (and `land.auto_lands()` with `auto_merge = false`).
    if repo.run.auto_merge && !repo.run.land.auto_lands() {
        repo.run.land = LandMode::Auto;
    } else if repo.run.land.auto_lands() && !repo.run.auto_merge {
        repo.run.auto_merge = true;
    }
    if let Some(requested) = run.land {
        if origin == OverlayOrigin::CommandLine || requested < repo.run.land {
            repo.run.land = requested;
            repo.run.auto_merge = requested.auto_lands();
        } else if requested > repo.run.land {
            warnings.push(format!(
                "{source}: run.land = {} ignored: it may only be lowered by local or environment config (kept {})",
                requested.as_str(),
                repo.run.land.as_str()
            ));
        }
    }
    match run.auto_merge {
        Some(false) => {
            if repo.run.land.auto_lands() {
                repo.run.land = LandMode::Pr;
            }
            repo.run.auto_merge = false;
        }
        Some(true) if origin == OverlayOrigin::CommandLine => {
            if !repo.run.land.auto_lands() {
                repo.run.land = LandMode::Auto;
            }
            repo.run.auto_merge = true;
        }
        Some(true) if !repo.run.auto_merge => warnings.push(format!(
            "{source}: run.auto_merge = true ignored: it is turned on only in .autofactory/config.toml or by the command-line flag"
        )),
        Some(true) | None => {}
    }
    if let Some(max_parallel) = run.max_parallel {
        repo.run.max_parallel = max_parallel;
    }
    if let Some(run_timeout_secs) = run.run_timeout_secs {
        repo.run.run_timeout_secs = run_timeout_secs;
    }
    let lock_model_effort = origin == OverlayOrigin::Ambient
        && (repo.pipeline.plan_debate || repo.cross_audit.dual_review);
    if let Some(model) = run.model {
        if lock_model_effort {
            if model != repo.run.model {
                warnings.push(format!(
                    "{source}: run.model = {model:?} ignored while plan_debate or dual_review is active (kept {:?})",
                    repo.run.model
                ));
            }
        } else {
            repo.run.model = model;
        }
    }
    if let Some(effort) = run.effort {
        if lock_model_effort {
            if effort != repo.run.effort {
                warnings.push(format!(
                    "{source}: run.effort = {effort:?} ignored while plan_debate or dual_review is active (kept {:?})",
                    repo.run.effort
                ));
            }
        } else {
            repo.run.effort = effort;
        }
    }
    if let Some(env_pass) = run.env_pass
        && env_pass != repo.run.env_pass
    {
        warnings.push(format!("{source}: run.env_pass ignored: {TIER_TWO_ONLY}"));
    }
    if let Some(local) = run.max_open_review {
        let cur = repo.run.max_open_review;
        if local == 0 {
            if cur > 0 {
                warnings.push(format!(
                    "{source}: run.max_open_review = 0 ignored: cannot disable cap (kept {cur})"
                ));
            }
        } else if cur == 0 || local < cur {
            repo.run.max_open_review = local;
        } else if local > cur {
            warnings.push(format!(
                "{source}: run.max_open_review = {local} ignored: it may only be lowered (kept {cur})"
            ));
        }
    }

    let current = &mut repo.gate;
    if let Some(base_ref) = gate.base_ref
        && base_ref != current.base_ref
    {
        // The base decides which changes the gate sees at all; `HEAD` would hide every commit.
        warnings.push(format!(
            "{source}: gate.base_ref = {base_ref:?} ignored: only .autofactory/config.toml may set it (kept {:?})",
            current.base_ref
        ));
    }
    if let Some(globs) = gate.head_lock_globs {
        for glob in globs {
            if !current.head_lock_globs.contains(&glob) {
                current.head_lock_globs.push(glob);
            }
        }
    }
    if let Some(mode) = gate.store_compliance {
        if mode > current.store_compliance {
            current.store_compliance = mode;
        } else if mode < current.store_compliance {
            warnings.push(format!(
                "{source}: gate.store_compliance = {} ignored: it may only be raised (kept {})",
                mode_name(mode),
                mode_name(current.store_compliance)
            ));
        }
    }
    if let Some(sdk) = gate.min_android_target_sdk {
        if sdk > current.min_android_target_sdk {
            current.min_android_target_sdk = sdk;
        } else if sdk < current.min_android_target_sdk {
            warnings.push(format!(
                "{source}: gate.min_android_target_sdk = {sdk} ignored: it may only be raised (kept {})",
                current.min_android_target_sdk
            ));
        }
    }
    if let Some(timeout) = gate.command_timeout_secs {
        current.command_timeout_secs = timeout;
    }
    if let Some(idle) = gate.idle_timeout_secs {
        current.idle_timeout_secs = idle;
    }
    match gate.docs_fast_path {
        Some(false) => current.docs_fast_path = false,
        Some(true) if !current.docs_fast_path => {
            warnings.push(format!(
                "{source}: gate.docs_fast_path = true ignored: it may only be turned off (kept false)"
            ));
        }
        Some(true) | None => {}
    }
    if let Some(globs) = gate.docs_fast_path_exclude_globs {
        for glob in globs {
            if !current.docs_fast_path_exclude_globs.contains(&glob) {
                current.docs_fast_path_exclude_globs.push(glob);
            }
        }
    }
    if let Some(mode) = gate.red_green_oracle {
        if mode > current.red_green_oracle {
            current.red_green_oracle = mode;
        } else if mode < current.red_green_oracle {
            warnings.push(format!(
                "{source}: gate.red_green_oracle = {} ignored: it may only be raised (kept {})",
                mode.as_str(),
                current.red_green_oracle.as_str()
            ));
        }
    }
    if let Some(policy) = gate.oracle_compile_red_policy {
        if policy > current.oracle_compile_red_policy {
            current.oracle_compile_red_policy = policy;
        } else if policy < current.oracle_compile_red_policy {
            warnings.push(format!(
                "{source}: gate.oracle_compile_red_policy = {} ignored: it may only be raised (kept {})",
                policy.as_str(),
                current.oracle_compile_red_policy.as_str()
            ));
        }
    }
}
