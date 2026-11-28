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
//! minimal Seatbelt profile that denies `AAF_DATA_DIR`, `~/.ssh`, `~/.aws`,
//! `~/.gnupg`, and `task_cwd` BEFORE reopening ONLY the host-private empty
//! `cwd` (`0700`, exclusive `mkdir`) and `~/Library/Keychains`, with zero tools,
//! zero MCP servers, `CLAUDE_CODE_SAFE_MODE=1` / `AGY_CLI_DISABLE_AUTO_UPDATE=1`,
//! cross-process file locks with failure backoff under `<data_dir>/oauth-refresh/`,
//! and full process-group timeout cleanup via [`crate::process::run_with_timeout`].

use aaf_domain::ports::{AgentRequest, InfraFailure, InfraKind};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::{ExecEnv, agent_env, should_deny_macos_keychain};

#[cfg(target_os = "macos")]
mod host;
#[cfg(target_os = "macos")]
mod seatbelt;

/// Threshold at which a 64 KiB PTY tail is considered byte-truncated so its
/// leading partial line (which may be a mid-JSON fragment) is discarded.
const TRUNCATED_TAIL_THRESHOLD_BYTES: usize = 60 * 1024;

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

struct NonEmptySecret(bool);

impl<'de> Deserialize<'de> for NonEmptySecret {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = NonEmptySecret;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("optional string")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(NonEmptySecret(!v.trim().is_empty()))
            }

            fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(NonEmptySecret(false))
            }

            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(NonEmptySecret(false))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

#[derive(Deserialize)]
struct ClaudeOAuthMeta {
    #[serde(rename = "expiresAt")]
    expires_at: i64,
    #[serde(rename = "refreshToken", default)]
    refresh_token: Option<NonEmptySecret>,
}

#[derive(Deserialize)]
struct ClaudeKeychainPayload {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: ClaudeOAuthMeta,
}

/// Parses `(expires_at_ms, has_refresh_token)` from Claude Keychain JSON
/// without allocating or retaining any `accessToken`/`refreshToken` strings.
#[must_use]
pub fn parse_claude_oauth_expiry_ms(raw_json: &str) -> Option<(i64, bool)> {
    let payload: ClaudeKeychainPayload = serde_json::from_str(raw_json.trim()).ok()?;
    let has_refresh = payload.claude_ai_oauth.refresh_token.is_some_and(|s| s.0);
    Some((payload.claude_ai_oauth.expires_at, has_refresh))
}

/// Filters out structured JSON object lines from a raw PTY tail so error
/// classifiers only inspect raw CLI stderr / crash lines, never assistant
/// prose, diffs, or tool results. When the tail reached the 64 KiB byte cap,
/// drops the potentially mid-line first fragment up to the first newline.
#[must_use]
pub fn non_json_lines(raw: &str) -> String {
    let body = if raw.len() >= TRUNCATED_TAIL_THRESHOLD_BYTES {
        raw.find('\n').map_or("", |i| &raw[i + 1..])
    } else {
        raw
    };
    body.lines()
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

/// True when a Claude run failed with a Keychain-save error OR a structured
/// `InfraKind::Auth` reporting an expired OAuth access token (which occurs when
/// the confined worker could not acquire Claude's credential-store lock to
/// refresh inline).
#[must_use]
pub fn is_claude_auth_retryable(infra: Option<&InfraFailure>, non_json_tail: &str) -> bool {
    if let Some(failure) = infra {
        if failure.kind != InfraKind::Auth {
            return false;
        }
        let lower = failure.raw.to_ascii_lowercase();
        if is_claude_keychain_save_error(&failure.raw)
            || lower.contains("oauth access token has expired")
            || lower.contains("oauth token has expired")
        {
            return true;
        }
    }
    is_claude_keychain_save_error(non_json_tail)
}

fn line_is_agy_keyring_or_login_error(line: &str) -> bool {
    const PERMANENT: &[&str] = &[
        "invalid_api_key",
        "invalid api key",
        "invalid_grant",
        "permission_denied",
    ];
    let lower = line.to_ascii_lowercase();
    !PERMANENT.iter().any(|p| lower.contains(p))
        && (lower.contains("not logged into antigravity")
            || lower.contains("failed keyring savetoken")
            || lower.contains("failed to save refreshed token")
            || lower.contains("keyring savetoken failed"))
}

/// True when an `agy` failure explicitly reports an Antigravity keyring/token-
/// save or login failure (rejecting generic `not logged in`, HTTP 401/403 from
/// unrelated test output, and permanent non-refreshable auth errors).
#[must_use]
pub fn is_agy_auth_retryable(infra: Option<&InfraFailure>, non_json_text: &str) -> bool {
    if let Some(failure) = infra {
        if failure.kind != InfraKind::Auth {
            return false;
        }
        if failure.raw.lines().any(line_is_agy_keyring_or_login_error) {
            return true;
        }
    }
    non_json_text
        .lines()
        .any(line_is_agy_keyring_or_login_error)
}

/// Converts a mid-run Keychain-save failure into a typed
/// [`InfraKind::OAuthRefreshed`] once host-side refresh succeeded and was
/// verified in Keychain, so the account is not quarantined for 1h and
/// `Mode::Drain`/`Mode::Once` can retry once cleanly without string matching.
#[must_use]
pub fn reclassify_refreshed_keychain_failure(
    original: Option<InfraFailure>,
) -> Option<InfraFailure> {
    let raw = original
        .map(|f| f.raw)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "mid-run OAuth expiry inside Seatbelt".to_string());
    Some(InfraFailure {
        kind: InfraKind::OAuthRefreshed,
        raw: format!("oauth refreshed on host after sandbox keychain write denial ({raw})"),
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
/// exists, has a refresh token, and is expired or expiring within 10 minutes.
/// Returns the observed Keychain `expiresAt` (ms) after preflight.
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
/// Keychain write denial. Returns `true` ONLY when Keychain `expiresAt` is
/// verified to have strictly advanced into the future.
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
/// `agy models` at most once every 40 minutes across processes (non-blocking).
/// Returns the observed Keychain `mdat` metadata baseline before the run so a
/// concurrent worker can detect a peer refresh.
pub(crate) fn preflight_agy(
    agy_bin: &str,
    env: &ExecEnv,
    request: &AgentRequest,
) -> Option<String> {
    let effective_env = agent_env(env, &[]);
    if !should_manage_keychain_oauth(env, request, "antigravity", &effective_env) {
        return None;
    }
    macos_impl::preflight_agy_impl(agy_bin, env, &request.cwd)
}

/// Runs host-side `agy models` once after a confirmed keyring/Antigravity auth
/// failure inside Seatbelt. Returns `true` ONLY when the Keychain item's
/// modification timestamp (`mdat`) actually changed (either by a peer since
/// `pre_run_mdat` or by this host refresh).
pub(crate) fn refresh_agy_now(
    agy_bin: &str,
    env: &ExecEnv,
    request: &AgentRequest,
    pre_run_mdat: Option<&str>,
) -> bool {
    let effective_env = agent_env(env, &[]);
    if !should_manage_keychain_oauth(env, request, "antigravity", &effective_env) {
        return false;
    }
    macos_impl::refresh_agy_now_impl(agy_bin, env, &request.cwd, pre_run_mdat)
}

/// Builds the minimal Seatbelt profile used by host-side OAuth refresh:
/// denies `data_dir`, `~/.ssh`, `~/.aws`, `~/.gnupg`, and `task_cwd` FIRST
/// (failing closed if any path cannot be quoted), then reopens ONLY `empty_cwd`
/// (with its directory entry write-denied), `keychain_dir`, and (for Claude)
/// the 3 OAuth refresh lock paths under `claude_config_dir`.
#[cfg(target_os = "macos")]
pub fn host_refresh_seatbelt_profile(
    empty_cwd: &std::path::Path,
    keychain_dir: &std::path::Path,
    claude_config_dir: Option<&std::path::Path>,
    denied_dirs: &[&std::path::Path],
) -> Option<String> {
    seatbelt::build_seatbelt_profile(empty_cwd, keychain_dir, claude_config_dir, denied_dirs)
}

/// Builds the full production host-refresh Seatbelt profile (including default
/// denied dirs such as `~/.ssh`, `~/.gemini`, `data_dir`, and `task_cwd`).
#[cfg(target_os = "macos")]
pub fn host_refresh_command_profile(
    env: &ExecEnv,
    home: &std::path::Path,
    task_cwd: &str,
    empty_cwd: &std::path::Path,
    claude_config_dir: Option<&std::path::Path>,
) -> Option<String> {
    seatbelt::build_host_sandbox_profile(env, home, task_cwd, empty_cwd, claude_config_dir)
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
    pub(super) fn preflight_agy_impl(_: &str, _: &ExecEnv, _: &str) -> Option<String> {
        None
    }
    pub(super) fn refresh_agy_now_impl(_: &str, _: &ExecEnv, _: &str, _: Option<&str>) -> bool {
        false
    }
}
