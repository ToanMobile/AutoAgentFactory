//! Declarative host-side worktree hydration configuration (`[worktree]`) and
//! shared secret-filename classification.
//!
//! `[worktree]` is strictly Tier-2 (`base_sha:.autofactory/config.toml` only)
//! and never accepted from Tier-3 local files or environment overlays.

use serde::{Deserialize, Serialize};

/// Host-side worktree hydration settings loaded from immutable `base_sha`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorktreeConfig {
    /// Gitignored build/dependency directories to copy-on-write clone into a
    /// fresh task worktree (e.g. `node_modules`, `Pods`, `Library`). Cloned
    /// destinations are placed in `SandboxPaths::write_denied` during agent
    /// turns so sandboxed agents cannot poison host-executed test runners.
    pub clone_dirs: Vec<String>,
    /// Non-secret gitignored files to copy into a fresh task worktree (e.g.
    /// `local.properties`). Secret-matching paths are rejected fail-closed.
    pub copy_files: Vec<String>,
    /// Gitignored files (including secrets such as `.env` or
    /// `google-services.json`) staged ONLY on the host immediately before
    /// `aaf gate` commands run and removed afterward. Never present during
    /// agent turns.
    pub gate_copy_files: Vec<String>,
    /// Optional host-side setup command executed once on a freshly created
    /// clean `base_sha` worktree (never after agent commits).
    pub setup_cmd: String,
}

impl WorktreeConfig {
    pub fn is_empty(&self) -> bool {
        self.clone_dirs.is_empty()
            && self.copy_files.is_empty()
            && self.gate_copy_files.is_empty()
            && self.setup_cmd.trim().is_empty()
    }
}

/// Non-secret `.env` templates that are explicitly exempt from secret-file
/// classification.
const SECRET_ENV_ALLOWLIST: &[&str] = &[".env.example", ".env.sample", ".env.template"];

/// Exact basenames treated as credential / secret files (mirrors `aaf-hook`).
const SECRET_EXACT_NAMES: &[&str] = &[
    "google-services.json",
    "googleservice-info.plist",
    "keystore.properties",
    "key.properties",
    "secrets.properties",
    "credentials.json",
    "service-account.json",
    "service_account.json",
    "id_rsa",
    "id_ed25519",
    "id_ecdsa",
    "id_dsa",
    ".npmrc",
    ".pypirc",
    ".netrc",
    "auth.json",
    "telegram.env",
    "receipt.key",
];

/// File extensions treated as credential / key material.
const SECRET_EXTENSIONS: &[&str] = &[
    ".pem",
    ".key",
    ".p12",
    ".p8",
    ".jks",
    ".keystore",
    ".mobileprovision",
    ".pfx",
];

/// True when `path` names a secret or credential file that must never be
/// copied into a sandboxed agent worktree via `[worktree].copy_files`.
pub fn is_secret_file(path: &str) -> bool {
    let trimmed = path.trim().trim_end_matches(['/', '\\']);
    let name = trimmed
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(trimmed)
        .to_ascii_lowercase();
    if name.is_empty() {
        return false;
    }
    if SECRET_ENV_ALLOWLIST.contains(&name.as_str()) {
        return false;
    }
    if name == ".env" || name.starts_with(".env.") || name.ends_with(".env") {
        return true;
    }
    if SECRET_EXACT_NAMES.contains(&name.as_str()) {
        return true;
    }
    if SECRET_EXTENSIONS.iter().any(|ext| name.ends_with(ext)) {
        return true;
    }
    if name.contains("secret")
        && (name.ends_with(".json")
            || name.ends_with(".yaml")
            || name.ends_with(".yml")
            || name.ends_with(".toml")
            || name.ends_with(".properties"))
    {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_file_classification_blocks_credentials_and_allows_templates() {
        for secret in [
            ".env",
            ".env.local",
            "app/google-services.json",
            "ios/GoogleService-Info.plist",
            "android/keystore.properties",
            "release.keystore",
            "certs/server.pem",
            "keys/id_ed25519",
            "telegram.env",
            ".npmrc",
        ] {
            assert!(is_secret_file(secret), "expected secret: {secret}");
        }
        for safe in [
            ".env.example",
            ".env.sample",
            ".env.template",
            "local.properties",
            "gradle.properties",
            "package.json",
            "src/main.rs",
        ] {
            assert!(!is_secret_file(safe), "expected non-secret: {safe}");
        }
    }
}
