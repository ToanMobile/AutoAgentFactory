//! Reads the three files, the environment and the command-line overrides
//! into one [`EffectiveConfig`].

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use aaf_domain::config::{GlobalConfig, RepoConfig, StoreMode};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::detect::PROFILES;
use super::merge::{
    ConfigOverlay, GateOverlay, OverlayOrigin, ProjectOverlay, RunOverlay, apply_overlay,
};

/// Tier 2, relative to the repo root.
pub const REPO_CONFIG_REL: &str = ".autofactory/config.toml";
/// Tier 3, relative to the repo root. Not committed.
pub const LOCAL_CONFIG_REL: &str = ".autofactory/config.local.toml";

const ENV_STORE_COMPLIANCE: &str = "AAF_STORE_COMPLIANCE";
const ENV_MIN_ANDROID_TARGET_SDK: &str = "AAF_MIN_ANDROID_TARGET_SDK";
const ENV_AUTO_MERGE: &str = "AAF_AUTO_MERGE";
const ENV_MAX_PARALLEL: &str = "AAF_MAX_PARALLEL";
/// Not supported on purpose; looked up only to tell the user they had no effect.
const ENV_COMMANDS: [&str; 5] = [
    "AAF_COMMANDS_BUILD",
    "AAF_COMMANDS_TEST_SCOPED",
    "AAF_COMMANDS_TEST_FULL",
    "AAF_COMMANDS_LINT",
    "AAF_COMMANDS_REPRO",
];

/// Everything [`load`] reads from the outside world, so tests never touch
/// the real home directory or process environment.
pub struct ConfigInputs<'a> {
    /// Tier 1 file; `None` when no config dir can be determined.
    pub global_config: Option<PathBuf>,
    pub repo_root: PathBuf,
    pub env: &'a dyn Fn(&str) -> Option<String>,
    /// Command-line overrides; `ConfigOverlay::default()` for none.
    pub cli: ConfigOverlay,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveConfig {
    pub global: GlobalConfig,
    pub repo: RepoConfig,
    /// The files that existed and were read, lowest tier first.
    pub sources: Vec<String>,
    /// Overrides that were ignored, each naming where it came from.
    pub warnings: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {}: {source}", .path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// `line` and `column` are 1-based; both are 0 when the parser gave no position.
    #[error("{}:{line}:{column}: {message}", .path.display())]
    Parse {
        path: PathBuf,
        line: usize,
        column: usize,
        message: String,
    },
    #[error("environment variable {var}={value:?} is invalid: expected {expected}")]
    InvalidEnv {
        var: String,
        value: String,
        expected: &'static str,
    },
    #[error("project.profile = {value:?} is not a known profile: expected auto, {}", PROFILES.join(", "))]
    InvalidProfile { value: String },
}

/// Tier 3 file: the overlay plus the sections it is not allowed to carry.
#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct LocalFile {
    project: ProjectOverlay,
    gate: GateOverlay,
    run: RunOverlay,
    commands: Option<toml::Value>,
    unblocker: Option<toml::Value>,
    safety: Option<toml::Value>,
    accounts: Option<toml::Value>,
    notifications: Option<toml::Value>,
}

/// `AAF_CONFIG_DIR/config.toml`, else `$HOME/.config/autofactory/config.toml`.
pub fn global_config_path(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let dir = match env("AAF_CONFIG_DIR").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => {
            let home = env("HOME").filter(|v| !v.is_empty())?;
            PathBuf::from(home).join(".config").join("autofactory")
        }
    };
    Some(dir.join("config.toml"))
}

fn process_env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// [`load`] with the real environment and no command-line overrides.
pub fn load_from_env(repo_root: &Path) -> Result<EffectiveConfig, ConfigError> {
    load_from_env_with(repo_root, ConfigOverlay::default())
}

/// [`load`] with the real environment and the given command-line overrides.
pub fn load_from_env_with(
    repo_root: &Path,
    cli: ConfigOverlay,
) -> Result<EffectiveConfig, ConfigError> {
    load(&ConfigInputs {
        global_config: global_config_path(&process_env),
        repo_root: repo_root.to_path_buf(),
        env: &process_env,
        cli,
    })
}

pub fn load(inputs: &ConfigInputs<'_>) -> Result<EffectiveConfig, ConfigError> {
    let mut sources = Vec::new();
    let mut warnings = Vec::new();

    let global: GlobalConfig = match &inputs.global_config {
        Some(path) => read_tier(path, &mut sources)?.unwrap_or_default(),
        None => GlobalConfig::default(),
    };
    let mut repo: RepoConfig =
        read_tier(&inputs.repo_root.join(REPO_CONFIG_REL), &mut sources)?.unwrap_or_default();

    let local_path = inputs.repo_root.join(LOCAL_CONFIG_REL);
    if let Some(local) = read_tier::<LocalFile>(&local_path, &mut sources)? {
        let source = local_path.display().to_string();
        let tier_two_only = [
            ("commands", &local.commands, REPO_CONFIG_REL),
            ("unblocker", &local.unblocker, REPO_CONFIG_REL),
            ("safety", &local.safety, "the global config"),
            ("accounts", &local.accounts, "the global config"),
            ("notifications", &local.notifications, "the global config"),
        ];
        for (section, value, owner) in tier_two_only {
            if value.is_some() {
                warnings.push(format!(
                    "{source}: [{section}] ignored: it is read from {owner} only"
                ));
            }
        }
        let overlay = ConfigOverlay {
            project: local.project,
            gate: local.gate,
            run: local.run,
        };
        apply_overlay(
            &mut repo,
            overlay,
            &source,
            OverlayOrigin::Ambient,
            &mut warnings,
        );
    }

    let env_overlay = env_overlay(inputs.env, &mut warnings)?;
    apply_overlay(
        &mut repo,
        env_overlay,
        "environment",
        OverlayOrigin::Ambient,
        &mut warnings,
    );
    apply_overlay(
        &mut repo,
        inputs.cli.clone(),
        "command line",
        OverlayOrigin::CommandLine,
        &mut warnings,
    );

    // A misspelt profile would otherwise silently select no store-compliance rules.
    let profile = &repo.project.profile;
    if profile != "auto" && !PROFILES.contains(&profile.as_str()) {
        return Err(ConfigError::InvalidProfile {
            value: profile.clone(),
        });
    }

    Ok(EffectiveConfig {
        global,
        repo,
        sources,
        warnings,
    })
}

/// `Ok(None)` only when the file does not exist. A file that exists but
/// cannot be read or parsed is an error: falling back to defaults would
/// silently drop whatever restrictions it was meant to carry.
fn read_tier<T: DeserializeOwned>(
    path: &Path,
    sources: &mut Vec<String>,
) -> Result<Option<T>, ConfigError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ConfigError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let value = toml::from_str::<T>(&text).map_err(|err| {
        let (line, column) = err
            .span()
            .map_or((0, 0), |span| line_column(&text, span.start));
        ConfigError::Parse {
            path: path.to_path_buf(),
            line,
            column,
            message: err.message().to_string(),
        }
    })?;
    sources.push(path.display().to_string());
    Ok(Some(value))
}

/// 1-based line and column (in characters) of a byte offset.
fn line_column(text: &str, offset: usize) -> (usize, usize) {
    let before = &text.as_bytes()[..offset.min(text.len())];
    let line = before.iter().filter(|b| **b == b'\n').count() + 1;
    let line_start = before
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(0, |pos| pos + 1);
    let column = String::from_utf8_lossy(&before[line_start..])
        .chars()
        .count()
        + 1;
    (line, column)
}

fn env_overlay(
    env: &dyn Fn(&str) -> Option<String>,
    warnings: &mut Vec<String>,
) -> Result<ConfigOverlay, ConfigError> {
    let mut overlay = ConfigOverlay::default();
    overlay.gate.store_compliance =
        env_parsed(env, ENV_STORE_COMPLIANCE, "off, auto or always", |v| {
            StoreMode::parse(v)
        })?;
    overlay.gate.min_android_target_sdk = env_parsed(
        env,
        ENV_MIN_ANDROID_TARGET_SDK,
        "a non-negative integer",
        |v| v.parse().ok(),
    )?;
    overlay.run.auto_merge = env_parsed(env, ENV_AUTO_MERGE, "true or false", parse_bool)?;
    overlay.run.max_parallel = env_parsed(env, ENV_MAX_PARALLEL, "a non-negative integer", |v| {
        v.parse().ok()
    })?;
    for var in ENV_COMMANDS {
        if env(var).is_some() {
            warnings.push(format!(
                "environment: {var} ignored: commands are read from {REPO_CONFIG_REL} only"
            ));
        }
    }
    Ok(overlay)
}

/// Unset and empty both mean "not given"; anything else must parse.
fn env_parsed<T>(
    env: &dyn Fn(&str) -> Option<String>,
    var: &str,
    expected: &'static str,
    parse: impl Fn(&str) -> Option<T>,
) -> Result<Option<T>, ConfigError> {
    let Some(raw) = env(var) else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    match parse(trimmed) {
        Some(value) => Ok(Some(value)),
        None => Err(ConfigError::InvalidEnv {
            var: var.to_string(),
            value: raw,
            expected,
        }),
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}
