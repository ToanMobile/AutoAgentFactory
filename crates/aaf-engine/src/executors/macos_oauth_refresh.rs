//! Host-side macOS OAuth pre-flight & reactive refresh for sandboxed `claude`
//! and `agy` runs.
//!
//! Seatbelt intentionally denies all writes to `~/Library/Keychains/` and
//! `~/.gemini/antigravity-cli/antigravity-oauth-token` so a confined agent can
//! never modify Keychain items or auth files. When a short-lived OAuth access
//! token expires (~8h for Claude, ~1h for Antigravity), the CLI inside
//! `sandbox-exec` cannot save the refreshed token back to Keychain and fails.
//!
//! This module refreshes expired/near-expiry tokens **on the host** inside a
//! minimal Seatbelt profile that permits writes ONLY to a host-private empty
//! `cwd` (`0700`, exclusive `mkdir`) and `~/Library/Keychains`, with zero tools,
//! zero MCP servers, `CLAUDE_CODE_SAFE_MODE=1` / `AGY_CLI_DISABLE_AUTO_UPDATE=1`,
//! cross-process file locks under `<data_dir>/oauth-refresh/`, and full process-
//! group timeout cleanup via [`crate::process::run_with_timeout`].

use aaf_domain::ports::{AgentRequest, InfraFailure, InfraKind};
use sha2::{Digest, Sha256};

use super::{ExecEnv, agent_env, should_deny_macos_keychain};

#[cfg(target_os = "macos")]
mod host;

/// Stable marker emitted when a mid-run sandbox Keychain write denial was
/// recovered by host-side OAuth refresh (allows `Mode::Drain` to retry once
/// without halting the factory).
pub const OAUTH_REFRESHED_MARKER: &str =
    "oauth refreshed on host after sandbox keychain write denial";

/// Computes the macOS Keychain service name Claude Code uses for `config_dir`.
///
/// Matches Claude Code's `Claude Code-credentials-<sha256(config_dir)[..8]>`
/// convention when `CLAUDE_CONFIG_DIR` is set, or `Claude Code-credentials`
/// for the default profile.
#[must_use]
pub fn claude_keychain_service(config_dir: Option<&str>) -> String {
    match config_dir.filter(|s| !s.trim().is_empty()) {
        Some(dir) => format!("Claude Code-credentials-{}", sha256_prefix_hex(dir, 4)),
        None => "Claude Code-credentials".to_string(),
    }
}

fn sha256_prefix_hex(input: &str, bytes: usize) -> String {
    let digest = Sha256::digest(input.as_bytes());
    let mut hex = String::with_capacity(bytes * 2);
    for byte in digest.iter().take(bytes) {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Parses `(expires_at_ms, has_refresh_token)` from Claude Keychain JSON
/// without retaining or logging any secret fields.
#[must_use]
pub fn parse_claude_oauth_expiry_ms(raw_json: &str) -> Option<(i64, bool)> {
    let val: serde_json::Value = serde_json::from_str(raw_json.trim()).ok()?;
    let oauth = val.get("claudeAiOauth")?;
    let expires_at = oauth.get("expiresAt")?.as_i64()?;
    let has_refresh = oauth
        .get("refreshToken")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|s| !s.trim().is_empty());
    Some((expires_at, has_refresh))
}

/// Filters out structured JSON object lines from a raw PTY tail so error
/// classifiers only inspect raw CLI stderr / crash lines, never assistant
/// prose, diffs, or tool results.
#[must_use]
pub fn non_json_lines(raw: &str) -> String {
    raw.lines()
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty()
                && !matches!(
                    serde_json::from_str::<serde_json::Value>(trimmed),
                    Ok(serde_json::Value::Object(_))
                )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// True when a single CLI error line indicates a Claude OAuth refresh failed
/// specifically because writing to the macOS Keychain was blocked.
///
/// Matches line-by-line so unrelated prose or diff hunks hundreds of lines
/// apart can never trigger a false positive.
#[must_use]
pub fn is_claude_keychain_save_error(text: &str) -> bool {
    text.lines().any(|line| {
        let lower = line.to_ascii_lowercase();
        lower.contains("can't save to the keychain")
            || lower.contains("cannot save to the keychain")
            || (lower.contains("failed to refresh oauth token") && lower.contains("keychain"))
            || (lower.contains("could not refresh your login") && lower.contains("keychain"))
            || (lower.contains("oauth token") && lower.contains("save to the keychain"))
    })
}

/// True when an `agy` failure is plausibly a refreshable OAuth/keyring error
/// (excluding permanent auth errors such as `invalid_api_key`, `invalid_grant`,
/// or `permission_denied`).
#[must_use]
pub fn is_agy_auth_retryable(infra: Option<&InfraFailure>, non_json_text: &str) -> bool {
    const PERMANENT: &[&str] = &[
        "invalid_api_key",
        "invalid api key",
        "invalid_grant",
        "permission_denied",
    ];
    if let Some(failure) = infra
        && failure.kind == InfraKind::Auth
    {
        let lower = failure.raw.to_ascii_lowercase();
        return !PERMANENT.iter().any(|p| lower.contains(p));
    }
    non_json_text.lines().any(|line| {
        let lower = line.to_ascii_lowercase();
        !PERMANENT.iter().any(|p| lower.contains(p))
            && (lower.contains("not logged into antigravity")
                || lower.contains("failed keyring savetoken")
                || lower.contains("failed to save refreshed token")
                || lower.contains("keyring savetoken failed"))
    })
}

/// Converts a mid-run Keychain-save `InfraKind::Auth` failure into a transient
/// retryable `InfraKind::Network` failure once host-side refresh succeeded, so
/// the account is not placed in a 1-hour auth cooldown and the factory does not
/// halt unnecessarily.
#[must_use]
pub fn reclassify_refreshed_keychain_failure(
    original: Option<InfraFailure>,
) -> Option<InfraFailure> {
    let raw = original
        .map(|f| f.raw)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "mid-run OAuth expiry inside Seatbelt".to_string());
    Some(InfraFailure {
        kind: InfraKind::Network,
        raw: format!("{OAUTH_REFRESHED_MARKER} ({raw})"),
    })
}

fn oauth_refresh_disabled() -> bool {
    std::env::var_os("AAF_DISABLE_OAUTH_REFRESH").is_some()
}

fn resolved_claude_config_dir(
    request: &AgentRequest,
    effective_env: &[(String, String)],
) -> Option<String> {
    if let Some(acc) = &request.account
        && !acc.config_dir.trim().is_empty()
    {
        return Some(acc.config_dir.clone());
    }
    effective_env
        .iter()
        .rev()
        .find(|(k, _)| k == "CLAUDE_CONFIG_DIR")
        .map(|(_, v)| v.clone())
        .filter(|v| !v.trim().is_empty())
}

fn should_manage_keychain_oauth(
    env: &ExecEnv,
    request: &AgentRequest,
    executor: &str,
    effective_env: &[(String, String)],
) -> bool {
    cfg!(target_os = "macos")
        && request.sandbox.enabled
        && !oauth_refresh_disabled()
        && !should_deny_macos_keychain(env.keychain_policy, executor, effective_env)
}

/// Proactively refreshes a Claude OAuth token on the host if its Keychain entry
/// exists, has a refresh token, and is expired or expiring within 5 minutes.
/// Returns the observed Keychain `expiresAt` (ms) after preflight so reactive
/// recovery only triggers when a refresh genuinely occurred.
pub(crate) fn preflight_claude(
    claude_bin: &str,
    env: &ExecEnv,
    request: &AgentRequest,
    passed_env: &[(String, String)],
) -> Option<i64> {
    let effective_env = agent_env(env, passed_env);
    if !should_manage_keychain_oauth(env, request, "claude", &effective_env) {
        return None;
    }
    let config_dir = resolved_claude_config_dir(request, &effective_env);
    macos_impl::preflight_claude_impl(claude_bin, env, &request.cwd, config_dir.as_deref())
}

/// Runs a single host-side Claude OAuth refresh after a sandboxed run hit a
/// Keychain write denial. Returns `true` ONLY when an expired/stale token was
/// actually refreshed (or advanced past `pre_run_expiry_ms` by a peer) and is
/// verified valid in Keychain afterward.
pub(crate) fn refresh_claude_now(
    claude_bin: &str,
    env: &ExecEnv,
    request: &AgentRequest,
    passed_env: &[(String, String)],
    pre_run_expiry_ms: Option<i64>,
) -> bool {
    let effective_env = agent_env(env, passed_env);
    if !should_manage_keychain_oauth(env, request, "claude", &effective_env) {
        return false;
    }
    let config_dir = resolved_claude_config_dir(request, &effective_env);
    macos_impl::refresh_claude_now_impl(
        claude_bin,
        env,
        &request.cwd,
        config_dir.as_deref(),
        pre_run_expiry_ms,
    )
}

/// Proactively refreshes Antigravity's OAuth token via minimal-Seatbelt host
/// `agy models` at most once every 40 minutes across processes.
pub(crate) fn preflight_agy(agy_bin: &str, env: &ExecEnv, request: &AgentRequest) {
    let effective_env = agent_env(env, &[]);
    if !should_manage_keychain_oauth(env, request, "antigravity", &effective_env) {
        return;
    }
    macos_impl::preflight_agy_impl(agy_bin, env, &request.cwd);
}

/// Runs host-side `agy models` once (bypassing the 40-minute stamp) after an
/// auth/keyring failure inside Seatbelt. Returns `true` if `agy models` exited 0.
pub(crate) fn refresh_agy_now(agy_bin: &str, env: &ExecEnv, request: &AgentRequest) -> bool {
    let effective_env = agent_env(env, &[]);
    if !should_manage_keychain_oauth(env, request, "antigravity", &effective_env) {
        return false;
    }
    macos_impl::refresh_agy_now_impl(agy_bin, env, &request.cwd)
}

/// Builds the minimal Seatbelt profile used by host-side OAuth refresh:
/// denies every file write except standard devices, `empty_cwd`, and
/// `keychain_dir`, and read/write-denies `data_dir`, `~/.ssh`, and `task_cwd`.
#[cfg(target_os = "macos")]
pub fn host_refresh_seatbelt_profile(
    empty_cwd: &std::path::Path,
    keychain_dir: &std::path::Path,
    data_dir: Option<&std::path::Path>,
    ssh_dir: Option<&std::path::Path>,
    task_cwd: Option<&std::path::Path>,
) -> Option<String> {
    host::build_seatbelt_profile(empty_cwd, keychain_dir, data_dir, ssh_dir, task_cwd)
}

#[cfg(target_os = "macos")]
use host as macos_impl;

#[cfg(not(target_os = "macos"))]
mod macos_impl {
    use super::*;

    pub(super) fn preflight_claude_impl(
        _: &str,
        _: &ExecEnv,
        _: &str,
        _: Option<&str>,
    ) -> Option<i64> {
        None
    }
    pub(super) fn refresh_claude_now_impl(
        _: &str,
        _: &ExecEnv,
        _: &str,
        _: Option<&str>,
        _: Option<i64>,
    ) -> bool {
        false
    }
    pub(super) fn preflight_agy_impl(_: &str, _: &ExecEnv, _: &str) {}
    pub(super) fn refresh_agy_now_impl(_: &str, _: &ExecEnv, _: &str) -> bool {
        false
    }
}
