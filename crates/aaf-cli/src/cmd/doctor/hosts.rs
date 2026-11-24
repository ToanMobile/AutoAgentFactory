//! Checks for multi-host hook adapters (Codex, Gemini, Antigravity, OpenCode),
//! configured executor hooks, Linux bubblewrap/container isolation, and Tier-1
//! Telegram remote companion.

use std::fs;
use std::path::Path;

use aaf_domain::config::RepoConfig;
use aaf_domain::telegram::TelegramConfig;
use aaf_engine::config::EffectiveConfig;
use aaf_engine::executors::linux_sandbox::{self, LinuxIsolation};
use aaf_engine::factory::stage_router::writer_hook_executors;
use aaf_engine::factory::{configured_audit_provider, resolve_auto_audit_executor, resolve_base};
use aaf_engine::github_board::has_trusted_login;
use aaf_engine::hook_settings::{
    check_base_for_executor, check_worktree_for_executor,
    multi_host::{HookHostTarget, HostHookStatus, check_host_if_present},
};
use serde_json::Value;

use super::{Check, advisory, not_applicable, required};
use crate::ctx::Ctx;

/// Returns the three repository-scoped executor/multi-host hook checks.
pub(super) fn repo_hook_checks(
    ctx: &Ctx,
    root_opt: Option<&Path>,
    repo: &RepoConfig,
) -> [Check; 3] {
    let Some(root) = root_opt else {
        return [
            not_applicable("multi_host_hooks"),
            not_applicable("executor_hooks"),
            not_applicable("readonly_executor_hooks"),
        ];
    };
    [
        advisory("multi_host_hooks", multi_host_hooks(root)),
        required("executor_hooks", executor_hooks(ctx, root, repo)),
        advisory(
            "readonly_executor_hooks",
            readonly_executor_hooks(ctx, root, repo),
        ),
    ]
}

fn resolve_base_sha(repo_root: &Path, base_ref: &str) -> Option<String> {
    let base = resolve_base(repo_root, base_ref).ok()?;
    aaf_engine::git::cmd::git(repo_root, &["rev-parse", "--verify", &base.start_point()]).ok()
}

fn check_executor_hook(
    ctx: &Ctx,
    repo_root: &Path,
    base_sha: Option<&str>,
    exec: &str,
) -> Result<(), String> {
    match base_sha {
        Some(sha) => check_base_for_executor(repo_root, sha, &ctx.path_var, exec),
        None => check_worktree_for_executor(repo_root, &ctx.path_var, None, exec),
    }
}

fn is_hooked_non_claude(executor: &str) -> bool {
    matches!(
        executor.trim().to_ascii_lowercase().as_str(),
        "codex" | "gemini" | "antigravity" | "opencode"
    )
}

/// Required check mirroring `aaf run` (W5e): every non-Claude built-in
/// code-writing executor must have its host hook registered on the base commit.
pub(super) fn executor_hooks(
    ctx: &Ctx,
    repo_root: &Path,
    repo: &RepoConfig,
) -> Result<String, String> {
    let execs = writer_hook_executors(repo);
    if execs.is_empty() {
        return Ok("no non-claude code-writing executor".to_string());
    }
    let base_sha = resolve_base_sha(repo_root, &repo.gate.base_ref);
    let mut ok_parts = Vec::new();
    let mut bad_parts = Vec::new();
    for exec in &execs {
        match check_executor_hook(ctx, repo_root, base_sha.as_deref(), exec) {
            Ok(()) => {
                if base_sha.is_some() {
                    ok_parts.push(format!("{exec}: hook registered on base"));
                } else {
                    ok_parts.push(format!(
                        "{exec}: hook registered (base unresolved: checked working tree)"
                    ));
                }
            }
            Err(problem) => {
                if base_sha.is_some() {
                    bad_parts.push(format!("{exec}: {problem}"));
                } else {
                    bad_parts.push(format!(
                        "{exec}: {problem} (base unresolved: checked working tree)"
                    ));
                }
            }
        }
    }
    if !bad_parts.is_empty() {
        Err(bad_parts.join("; "))
    } else {
        Ok(ok_parts.join("; "))
    }
}

/// Advisory check for explicitly configured read-only stages using non-Claude
/// hooked executors (`codex`, `gemini`, `antigravity`, `opencode`) without a
/// custom `executor_cmd`. Auto/fallback resolution is intentionally not modeled.
pub(super) fn readonly_executor_hooks(
    ctx: &Ctx,
    repo_root: &Path,
    repo: &RepoConfig,
) -> Result<String, String> {
    const SCOPE_NOTE: &str = "explicit read-only executors only; auto/fallback not inspected";
    let mut targets: Vec<(String, String)> = Vec::new();
    if repo.pipeline.plan_debate {
        for (stage_name, stage) in [
            ("pipeline.plan_author", &repo.pipeline.plan_author),
            ("pipeline.plan_challenger", &repo.pipeline.plan_challenger),
        ] {
            if !stage.is_empty()
                && stage.executor_cmd.trim().is_empty()
                && is_hooked_non_claude(&stage.executor)
            {
                targets.push((
                    stage_name.to_string(),
                    stage.executor.trim().to_ascii_lowercase(),
                ));
            }
        }
    }
    if repo.cross_audit.dual_review {
        for (idx, reviewer) in repo.cross_audit.reviewers.iter().enumerate() {
            if reviewer.executor_cmd.trim().is_empty() && is_hooked_non_claude(&reviewer.executor) {
                let stage_name = if reviewer.id.trim().is_empty() {
                    format!("cross_audit.reviewers[{idx}]")
                } else {
                    format!("cross_audit.reviewers.{}", reviewer.id.trim())
                };
                targets.push((stage_name, reviewer.executor.trim().to_ascii_lowercase()));
            }
        }
    }
    if repo.cross_audit.executor_cmd.trim().is_empty()
        && is_hooked_non_claude(&repo.cross_audit.executor)
    {
        targets.push((
            "cross_audit.executor".to_string(),
            repo.cross_audit.executor.trim().to_ascii_lowercase(),
        ));
    }

    if targets.is_empty() {
        return Ok(format!(
            "no unguarded explicit read-only executor ({SCOPE_NOTE})"
        ));
    }

    let base_sha = resolve_base_sha(repo_root, &repo.gate.base_ref);
    let mut ok_parts = Vec::new();
    let mut bad_parts = Vec::new();
    for (stage, exec) in targets {
        match check_executor_hook(ctx, repo_root, base_sha.as_deref(), &exec) {
            Ok(()) => {
                if base_sha.is_some() {
                    ok_parts.push(format!("{exec} ({stage}): hook registered on base"));
                } else {
                    ok_parts.push(format!(
                        "{exec} ({stage}): hook registered (base unresolved: checked working tree)"
                    ));
                }
            }
            Err(_) => {
                let mut msg = format!(
                    "{exec} ({stage}) runs without aaf-hook: register with \
                     `aaf init --hook-host {exec}` — not enforced by aaf run yet"
                );
                if base_sha.is_none() {
                    msg.push_str(" (base unresolved: checked working tree)");
                }
                bad_parts.push(msg);
            }
        }
    }
    if !bad_parts.is_empty() {
        Err(bad_parts.join("; "))
    } else {
        Ok(format!("{} ({SCOPE_NOTE})", ok_parts.join("; ")))
    }
}

fn inspect_single_host(
    repo_root: &Path,
    host: HookHostTarget,
    ok_parts: &mut Vec<String>,
    bad_parts: &mut Vec<String>,
) {
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

/// Reports Codex, Gemini/Antigravity, and OpenCode hook registrations when
/// configured in `repo_root`.
pub(super) fn multi_host_hooks(repo_root: &Path) -> Result<String, String> {
    let mut ok_parts = Vec::new();
    let mut bad_parts = Vec::new();

    inspect_single_host(
        repo_root,
        HookHostTarget::Codex,
        &mut ok_parts,
        &mut bad_parts,
    );
    match (
        check_host_if_present(repo_root, HookHostTarget::Gemini),
        check_host_if_present(repo_root, HookHostTarget::Antigravity),
    ) {
        (HostHookStatus::Registered(reg), _) => {
            ok_parts.push(format!(
                "gemini={} ({})",
                reg.command,
                HookHostTarget::Gemini.settings_rel()
            ));
        }
        (_, HostHookStatus::Registered(reg)) => {
            ok_parts.push(format!(
                "antigravity={} ({}; covers gemini tools)",
                reg.command,
                HookHostTarget::Antigravity.settings_rel()
            ));
        }
        (HostHookStatus::NotConfigured, HostHookStatus::NotConfigured) => {}
        (HostHookStatus::Invalid(reason), HostHookStatus::Invalid(_))
        | (HostHookStatus::Invalid(reason), HostHookStatus::NotConfigured)
        | (HostHookStatus::NotConfigured, HostHookStatus::Invalid(reason)) => {
            bad_parts.push(format!(
                "{}: {reason}",
                HookHostTarget::Gemini.settings_rel()
            ));
        }
    }
    inspect_single_host(
        repo_root,
        HookHostTarget::Opencode,
        &mut ok_parts,
        &mut bad_parts,
    );

    if !bad_parts.is_empty() {
        return Err(bad_parts.join("; "));
    }
    if ok_parts.is_empty() {
        Ok(
            "claude only (pass `aaf init --hook-host all` to register codex/gemini/opencode; \
             `--hook-host antigravity` for Antigravity)"
                .to_string(),
        )
    } else {
        Ok(ok_parts.join("; "))
    }
}

/// Reports host gate-command confinement readiness without touching `data_dir`.
pub(super) fn gate_confinement_check() -> Result<String, String> {
    aaf_engine::gate::confine::probe()
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
pub(super) fn executor_token_budget(ctx: &Ctx, repo: &RepoConfig) -> Result<String, String> {
    let run_exec = repo.run.executor.trim();
    // `auto` (what `aaf init` writes) is resolved the way `aaf run` resolves
    // it: to another installed CLI, else to `[run].executor`.
    let audit_exec = resolve_auto_audit_executor(
        run_exec,
        configured_audit_provider(run_exec, &repo.cross_audit),
        &ctx.path_var,
        &ctx.home,
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

    #[test]
    fn gate_confinement_check_reports_host_status() {
        let res = gate_confinement_check();
        #[cfg(target_os = "macos")]
        {
            if Path::new("/usr/bin/sandbox-exec").is_file() {
                let detail = res.expect("expected macOS sandbox-exec to be available");
                assert!(detail.contains("/usr/bin/sandbox-exec"), "{detail}");
            } else {
                assert!(res.is_err());
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let detail = match res {
                Ok(ok) => ok,
                Err(err) => err,
            };
            assert!(!detail.is_empty());
        }
    }
}
