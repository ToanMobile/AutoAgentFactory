//! Three-tier configuration (docs/PLAN.md C.4).
//!
//! Tier 1 global file < tier 2 committed repo file < tier 3 local file <
//! `AAF_*` environment < command line. Everything above tier 2 may only
//! tighten `gate.*` and never supplies `commands.*`, `project.profile`,
//! `run.executor` or `run.executor_cmd`, because those tiers are writable by
//! an agent working in the checkout without leaving a diff. `run.auto_merge`
//! can be switched off anywhere, and on only in tier 2 or by a CLI flag.

mod detect;
mod detect_unity;
mod load;
mod merge;
mod render;

pub use detect::{Detected, PROFILES, detect_stack, effective_profile};
pub use load::{
    ConfigError, ConfigInputs, EffectiveConfig, LOCAL_CONFIG_REL, REPO_CONFIG_REL,
    global_config_path, load, load_from_env, load_from_env_with, load_over_repo,
    load_over_repo_from_env,
};
pub use merge::{ConfigOverlay, GateOverlay, ProjectOverlay, RunOverlay};
pub use render::render_repo_config;
