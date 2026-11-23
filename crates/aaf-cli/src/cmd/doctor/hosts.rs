//! Advisory checks for multi-host hook adapters (Codex, Gemini), Linux
//! bubblewrap/container isolation, and Tier-1 Telegram remote companion.

use std::path::Path;

use aaf_domain::config::RepoConfig;
use aaf_domain::telegram::TelegramConfig;
use aaf_engine::executors::linux_sandbox;
use aaf_engine::factory::configured_audit_provider;
use aaf_engine::hook_settings::multi_host::{
    HookHostTarget, HostHookStatus, check_host_if_present,
};

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
    let iso = linux_sandbox::detect_host_linux_isolation();
    if iso.is_isolated() {
        Ok(iso.summary())
    } else {
        Err(format!("{}; aaf run needs --unsandboxed", iso.summary()))
    }
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

/// Warns when a generic executor (which does not report token usage) is paired
/// with a positive `unblocker.max_token_budget`.
pub(super) fn executor_token_budget(repo: &RepoConfig) -> Result<String, String> {
    let run_exec = repo.run.executor.trim();
    let audit_exec = configured_audit_provider(run_exec, &repo.cross_audit);
    let mut generic = Vec::new();
    if run_exec != "claude" {
        generic.push(format!("run.executor={run_exec:?}"));
    }
    if audit_exec != "claude" && audit_exec != run_exec {
        generic.push(format!("cross_audit.executor={audit_exec:?}"));
    }
    if generic.is_empty() {
        return Ok("claude reports token usage; max_token_budget enforced".to_string());
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
