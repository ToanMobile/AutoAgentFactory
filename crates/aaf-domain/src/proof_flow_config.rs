//! Configuration types and validation for declarative proof flows (`[[proof.flows]]`)
//! and Tier-1 secret allowlisting (`[proof].secret_env_allowlist`).

use std::collections::BTreeSet;
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

/// Default per-flow timeout in seconds when omitted in `[[proof.flows]]`.
pub const DEFAULT_FLOW_TIMEOUT_SECS: u64 = 180;
/// Lower bound for per-flow timeout clamping (seconds).
pub const MIN_FLOW_TIMEOUT_SECS: u64 = 10;
/// Upper bound for per-flow timeout clamping (seconds).
pub const MAX_FLOW_TIMEOUT_SECS: u64 = 900;
/// Maximum allowed length of a proof flow name.
pub const MAX_FLOW_NAME_LEN: usize = 40;

/// Tier-1 (`~/.config/autofactory/config.toml`) proof configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GlobalProofConfig {
    /// Environment variable names that proof flows are permitted to receive
    /// from the host `aaf` process environment.
    pub secret_env_allowlist: Vec<String>,
}

/// One repository-declared proof flow read strictly from `base_sha`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProofFlow {
    /// Flow identifier matching `[a-z0-9_-]{1,40}`, unique within `[proof].flows`.
    pub name: String,
    /// Repository-relative script path (blob in `base_sha`, no `..`, not absolute).
    pub script: String,
    /// Timeout in seconds (default 180, clamped to `10..=900` at execution time).
    pub timeout_secs: u64,
    /// When `true`, passing this flow still blocks automatic merge/landing and
    /// routes the task to human review.
    pub needs_human: bool,
    /// Environment variable names required by the script; every name must be
    /// explicitly allowlisted in Tier-1 `[proof].secret_env_allowlist`.
    pub secrets: Vec<String>,
}

impl Default for ProofFlow {
    fn default() -> Self {
        Self {
            name: String::new(),
            script: String::new(),
            timeout_secs: DEFAULT_FLOW_TIMEOUT_SECS,
            needs_human: false,
            secrets: Vec::new(),
        }
    }
}

impl ProofFlow {
    /// Returns `timeout_secs` clamped into `[MIN_FLOW_TIMEOUT_SECS, MAX_FLOW_TIMEOUT_SECS]`.
    pub fn effective_timeout_secs(&self) -> u64 {
        self.timeout_secs
            .clamp(MIN_FLOW_TIMEOUT_SECS, MAX_FLOW_TIMEOUT_SECS)
    }
}

/// Validates a proof flow name (`[a-z0-9_-]{1,40}`).
pub fn is_valid_flow_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_FLOW_NAME_LEN
        && name
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
}

/// Validates a repo-relative proof flow script path (non-empty, relative, no `..`,
/// no NUL or backslash).
pub fn validate_flow_script_path(script: &str) -> Result<(), String> {
    if script.is_empty() || script != script.trim() {
        return Err("proof flow script path must be non-empty without surrounding spaces".into());
    }
    if script.contains('\0') || script.contains('\\') || script.starts_with('/') {
        return Err(format!(
            "proof flow script {script:?} must be a clean repository-relative path"
        ));
    }
    let path = Path::new(script);
    if path.is_absolute() {
        return Err(format!(
            "proof flow script {script:?} must not be an absolute path"
        ));
    }
    let mut has_normal = false;
    for comp in path.components() {
        match comp {
            Component::Normal(_) => has_normal = true,
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!(
                    "proof flow script {script:?} must not contain '..' or root components"
                ));
            }
        }
    }
    if !has_normal {
        return Err(format!(
            "proof flow script {script:?} must reference a file inside the repository"
        ));
    }
    Ok(())
}

/// Validates a list of `[[proof.flows]]` entries loaded from `base_sha`.
pub fn validate_flows(flows: &[ProofFlow]) -> Result<(), String> {
    let mut seen_names = BTreeSet::new();
    for flow in flows {
        if !is_valid_flow_name(&flow.name) {
            return Err(format!(
                "invalid proof flow name {:?}: expected [a-z0-9_-]{{1,{MAX_FLOW_NAME_LEN}}}",
                flow.name
            ));
        }
        if !seen_names.insert(flow.name.as_str()) {
            return Err(format!("duplicate proof flow name {:?}", flow.name));
        }
        validate_flow_script_path(&flow.script)
            .map_err(|msg| format!("proof flow {}: {msg}", flow.name))?;
        let mut seen_secrets = BTreeSet::new();
        for secret in &flow.secrets {
            if secret.is_empty() || secret != secret.trim() {
                return Err(format!(
                    "proof flow {}: secret env name {secret:?} must be non-empty",
                    flow.name
                ));
            }
            if !seen_secrets.insert(secret.as_str()) {
                return Err(format!(
                    "proof flow {}: duplicate secret env name {secret:?}",
                    flow.name
                ));
            }
        }
    }
    Ok(())
}
