//! Advisory checks for multi-host hook adapters (Codex, Gemini), Linux
//! bubblewrap/container isolation, and Tier-1 Telegram remote companion.

use std::fs;
use std::path::Path;

use aaf_domain::config::RepoConfig;
use aaf_domain::telegram::TelegramConfig;
use aaf_engine::config::EffectiveConfig;
use aaf_engine::executors::linux_sandbox::{self, LinuxIsolation};
use aaf_engine::factory::{configured_audit_provider, resolve_auto_audit_executor};
use aaf_engine::github_board::has_trusted_login;
use aaf_engine::hook_settings::multi_host::{
    HookHostTarget, HostHookStatus, check_host_if_present,
};
use serde_json::Value;

/// Reports Codex and Gemini hook registrations when configured in `repo_root`.
pub(super) fn multi_host_hooks(repo_root: &Path) -> Result<String, String> {
    let mut ok_parts = Vec::new();
    let mut bad_parts = Vec::new();
    for host in [HookHostTarget::Codex, HookHostTarget::Gemini] {
        match check_host_if_present(repo_root, host) {
            HostHookStatus::Registered(reg) => {
                ok_parts.push(format!(
                    "{}={} ({})",
                    host.as_str(),
                    reg.command,
                    host.settings_rel()
                ));
            }
            HostHookStatus::NotConfigured => {}
            HostHookStatus::Invalid(reason) => {
                bad_parts.push(format!("{}: {reason}", host.settings_rel()));
            }
        }
    }
    if !bad_parts.is_empty() {
        return Err(bad_parts.join("; "));
    }
    if ok_parts.is_empty() {
        Ok("claude only (pass `aaf init --hook-host all` to register codex/gemini)".to_string())
    } else {
        Ok(ok_parts.join("; "))
    }
}

/// Reports Linux bubblewrap / container isolation on Linux hosts.
pub(super) fn linux_sandbox_check() -> Result<String, String> {
    linux_sandbox_report(&linux_sandbox::detect_host_linux_isolation())
}

/// `aaf run` does not wrap agents in bwrap yet (`run::sandbox_choice`
/// refuses off macOS without `--unsandboxed`), so what was detected is only
/// informational and the check always warns.
fn linux_sandbox_report(iso: &LinuxIsolation) -> Result<String, String> {
    Err(format!(
        "aaf run on Linux needs --unsandboxed (bwrap integration pending); detected: {}",
        iso.summary()
    ))
}

/// Reports Tier-1 Telegram companion readiness without exposing the bot token.
pub(super) fn telegram_status(cfg: &TelegramConfig) -> Result<String, String> {
    if !cfg.enabled {
        return Ok("disabled".to_string());
    }
    let mut issues = Vec::new();
    if cfg.allowed_chat_ids.is_empty() {
        issues.push("telegram.allowed_chat_ids is empty (fail-closed)".to_string());
    }
    let token_present = std::env::var(&cfg.bot_token_env)
        .ok()
        .is_some_and(|v| !v.trim().is_empty());
    if !token_present {
        issues.push(format!("env {} is not set", cfg.bot_token_env));
    }
    if issues.is_empty() {
        Ok(format!(
            "enabled ({} allowed chat(s), allow_approve={}, allow_mutating={})",
            cfg.allowed_chat_ids.len(),
            cfg.allow_approve,
            cfg.allow_mutating
        ))
    } else {
        Err(issues.join("; "))
    }
}

/// Reports installed coding-agent CLIs across the 10-CLI registry (no subprocesses).
pub(super) fn executors_summary(path_var: &std::ffi::OsStr) -> Result<String, String> {
    let inv = aaf_engine::executors::registry::discover_inventory(path_var);
    let installed: Vec<&str> = inv
        .iter()
        .filter(|item| item.installed)
        .map(|item| item.id.as_str())
        .collect();
    if installed.is_empty() {
        Err(format!("0/{} coding-agent CLIs found on PATH", inv.len()))
    } else {
        Ok(format!(
            "{}/{} installed ({})",
            installed.len(),
            inv.len(),
            installed.join(", ")
        ))
    }
}

/// Read-only `.mcp.json` inline-secret hygiene summary.
pub(super) fn mcp_hygiene(repo_root: &Path) -> Result<String, String> {
    let report = aaf_engine::roles::inspect_mcp_servers(repo_root)?;
    if !report.present {
        return Ok("no .mcp.json".to_string());
    }
    if report.has_warnings() {
        Err(format!(
            "{} server(s), {} inline-secret warning(s); run `aaf executors mcp`",
            report.servers.len(),
            report.total_warnings()
        ))
    } else {
        Ok(format!(
            "{} server(s), no inline secret warnings",
            report.servers.len()
        ))
    }
}

/// Validates `.mcp.json` configures `mobile-mcp` with telemetry disabled.
pub(super) fn check_mcp_mobile(root: &Path) -> Result<(), String> {
    let text = fs::read_to_string(root.join(".mcp.json"))
        .map_err(|_| "missing .mcp.json with mobile-mcp".to_string())?;
    let val: Value =
        serde_json::from_str(&text).map_err(|e| format!(".mcp.json is invalid JSON: {e}"))?;
    let srv = val
        .pointer("/mcpServers/mobile-mcp")
        .ok_or_else(|| ".mcp.json does not configure mobile-mcp".to_string())?;
    if srv
        .pointer("/env/MOBILEMCP_DISABLE_TELEMETRY")
        .and_then(Value::as_str)
        != Some("1")
    {
        return Err(".mcp.json mobile-mcp lacks MOBILEMCP_DISABLE_TELEMETRY=\"1\"".to_string());
    }
    Ok(())
}

/// Checks GitHub board integration readiness when enabled for the repository.
pub(super) fn github_board_status(cfg: &EffectiveConfig) -> Result<String, String> {
    if !cfg.repo.github.enabled {
        return Ok("disabled".to_string());
    }
    let allow = &cfg.global.safety.owner_github_allowlist;
    let mut issues = Vec::new();
    if !has_trusted_login(allow) {
        issues.push("safety.owner_github_allowlist has no valid owner login".to_string());
    }
    if let Err(err) = aaf_fp::program("gh") {
        issues.push(format!("gh CLI unavailable: {err}"));
    }
    if issues.is_empty() {
        let n = allow.iter().filter(|s| !s.trim().is_empty()).count();
        Ok(format!("enabled ({n} allowlisted owner(s), gh ready)"))
    } else {
        Err(issues.join("; "))
    }
}

fn reports_tokens(executor: &str) -> bool {
    matches!(
        executor.trim().to_ascii_lowercase().as_str(),
        "claude" | "antigravity"
    )
}

/// Warns when a generic executor (which does not report token usage) is paired
/// with a positive `unblocker.max_token_budget`.
pub(super) fn executor_token_budget(repo: &RepoConfig) -> Result<String, String> {
    let run_exec = repo.run.executor.trim();
    // `auto` (what `aaf init` writes) is resolved the way `aaf run` resolves
    // it: to another installed CLI, else to `[run].executor`.
    let home = std::env::var_os("HOME").unwrap_or_default();
    let audit_exec = resolve_auto_audit_executor(
        run_exec,
        configured_audit_provider(run_exec, &repo.cross_audit),
        &std::env::var_os("PATH").unwrap_or_default(),
        Path::new(&home),
    );
    let audit_exec = audit_exec.as_str();
    let mut generic = Vec::new();
    if !reports_tokens(run_exec) {
        generic.push(format!("run.executor={run_exec:?}"));
    }
    if !reports_tokens(audit_exec) && audit_exec != run_exec {
        generic.push(format!("cross_audit.executor={audit_exec:?}"));
    }
    if generic.is_empty() {
        return Ok("claude/antigravity report token usage; max_token_budget enforced".to_string());
    }
    if repo.unblocker.max_token_budget == 0 {
        return Ok(format!(
            "{}: max_token_budget is disabled (0)",
            generic.join(", ")
        ));
    }
    Err(format!(
        "{} does not report token usage so unblocker.max_token_budget={} is not enforced \
         (only max_wall_clock_secs and max_unblock_cycles apply)",
        generic.join(", "),
        repo.unblocker.max_token_budget
    ))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// R10-L7-2: `aaf run` on Linux never wraps agents in bwrap yet
    /// (`run::sandbox_choice` refuses without `--unsandboxed` off macOS), so
    /// doctor must not report a detected bwrap or container as a working
    /// sandbox.
    #[test]
    fn linux_sandbox_report_says_run_needs_unsandboxed_even_with_bwrap() {
        for iso in [
            LinuxIsolation::Bubblewrap {
                bwrap_bin: PathBuf::from("/usr/bin/bwrap"),
            },
            LinuxIsolation::HardenedContainer {
                indicator: "docker",
            },
            LinuxIsolation::None,
        ] {
            let Err(text) = linux_sandbox_report(&iso) else {
                panic!("reported as a working sandbox: {iso:?}");
            };
            assert!(
                text.contains("aaf run on Linux needs --unsandboxed"),
                "{text}"
            );
            assert!(text.contains("bwrap integration pending"), "{text}");
            assert!(text.contains(&iso.summary()), "{text}");
        }
    }
}
