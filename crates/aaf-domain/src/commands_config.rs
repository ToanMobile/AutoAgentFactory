//! Project metadata and Tier-2 `[commands]` configuration, extracted from
//! `config.rs` to keep that file under the 600-line repo limit.

use serde::{Deserialize, Serialize};

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
    /// Optional deterministic pre-gate formatter command (Tier 2 only; default empty).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub autofix: String,
    /// Command keys (e.g. `test_scoped`) that bypass the machine-wide heavy
    /// slot when their command text does not directly invoke a heavy build
    /// tool (useful for diff-aware wrapper scripts that call `aaf slot --`
    /// only on heavy branches). Tier 2 only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub light_commands: Vec<String>,
}
