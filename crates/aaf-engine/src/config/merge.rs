//! Overrides from tier 3 and above, and the rules for applying them.

use aaf_domain::config::{RepoConfig, StoreMode};
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
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunOverlay {
    pub executor: Option<String>,
    pub executor_cmd: Option<Vec<String>>,
    pub auto_merge: Option<bool>,
    pub max_parallel: Option<u32>,
    pub run_timeout_secs: Option<u64>,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Accepted so that it can be reported; never applied.
    pub env_pass: Option<Vec<String>>,
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
    match run.auto_merge {
        Some(false) => repo.run.auto_merge = false,
        Some(true) if origin == OverlayOrigin::CommandLine => repo.run.auto_merge = true,
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
    if let Some(model) = run.model {
        repo.run.model = model;
    }
    if let Some(effort) = run.effort {
        repo.run.effort = effort;
    }
    if let Some(env_pass) = run.env_pass
        && env_pass != repo.run.env_pass
    {
        warnings.push(format!("{source}: run.env_pass ignored: {TIER_TWO_ONLY}"));
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
}
