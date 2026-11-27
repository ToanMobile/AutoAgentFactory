//! `aaf executors list|resolve|roles|mcp`: inspect multi-CLI inventory, fallback
//! routing, role profiles, and read-only `.mcp.json` secret hygiene.

use aaf_engine::executors::registry::{discover_inventory_with_home, resolve_with_fallback};
use aaf_engine::roles::{inspect_mcp_servers, load_roles};
use serde_json::{Value, json};

use crate::ctx::Ctx;
use crate::output::{CmdError, CmdOutput, CmdResult};

#[derive(Debug, clap::Args)]
pub struct ExecutorsArgs {
    #[command(subcommand)]
    pub command: ExecutorsCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum ExecutorsCommand {
    /// List all 11 built-in coding-agent CLIs, installed status, binary path, and hook support.
    List,
    /// Preview which executor would be selected given a primary and ordered fallbacks.
    Resolve {
        /// Primary executor id or alias (e.g. `claude`, `codex`, `gemini`).
        primary: String,
        /// Ordered fallback executor ids or aliases; repeat `--fallback` for more.
        #[arg(long = "fallback")]
        fallback: Vec<String>,
    },
    /// List built-in starter roles overlaid with `.autofactory/roles/*.toml`.
    Roles,
    /// Audit `.mcp.json` servers read-only and flag inline secret leaks.
    Mcp,
}

pub fn run(args: &ExecutorsArgs, ctx: &Ctx) -> CmdResult {
    match &args.command {
        ExecutorsCommand::List => run_list(ctx),
        ExecutorsCommand::Resolve { primary, fallback } => run_resolve(primary, fallback, ctx),
        ExecutorsCommand::Roles => run_roles(ctx),
        ExecutorsCommand::Mcp => run_mcp(ctx),
    }
}

fn run_list(ctx: &Ctx) -> CmdResult {
    let inventory = discover_inventory_with_home(&ctx.path_var, Some(&ctx.home));
    let installed_count = inventory.iter().filter(|item| item.installed).count();

    let mut lines = Vec::with_capacity(inventory.len() + 2);
    lines.push(format!(
        "{:<14} {:<18} {:<9} {:<24} {}",
        "EXECUTOR", "NAME", "INSTALLED", "HOOK", "BINARY"
    ));
    for item in &inventory {
        let status = if item.installed { "yes" } else { "no" };
        let binary = item
            .resolved_binary
            .as_ref()
            .map_or_else(|| "-".to_string(), |p| p.display().to_string());
        lines.push(format!(
            "{:<14} {:<18} {:<9} {:<24} {}",
            item.id,
            item.display_name,
            status,
            item.hook_support.as_str(),
            binary
        ));
    }
    lines.push(format!("installed: {installed_count}/{}", inventory.len()));

    let executors_json = serde_json::to_value(&inventory)
        .map_err(|err| CmdError::new(format!("serialize executors: {err}")))?;

    Ok(CmdOutput::ok("ok", lines.join("\n"))
        .field("count", json!(inventory.len()))
        .field("installed_count", json!(installed_count))
        .field("executors", executors_json))
}

fn run_resolve(primary: &str, fallback: &[String], ctx: &Ctx) -> CmdResult {
    match resolve_with_fallback(primary, fallback, &ctx.path_var, &[]) {
        Ok(resolved) => {
            let mut lines = vec![
                format!(
                    "selected: {} ({})",
                    resolved.descriptor.id,
                    resolved.binary_path.display()
                ),
                format!("display_name: {}", resolved.descriptor.display_name),
                format!("used_fallback: {}", resolved.used_fallback),
                format!(
                    "hook_support: {}",
                    resolved.descriptor.hook_support.as_str()
                ),
            ];
            for (name, reason) in &resolved.skipped {
                lines.push(format!("skipped: {name} ({reason})"));
            }
            let skipped_json: Vec<Value> = resolved
                .skipped
                .iter()
                .map(|(executor, reason)| json!({"executor": executor, "reason": reason}))
                .collect();
            Ok(CmdOutput::ok("ok", lines.join("\n"))
                .field("primary", json!(primary))
                .field("fallbacks", json!(fallback))
                .field("selected", json!(resolved.descriptor.id))
                .field("display_name", json!(resolved.descriptor.display_name))
                .field(
                    "binary_path",
                    json!(resolved.binary_path.to_string_lossy().into_owned()),
                )
                .field("hook_support", json!(resolved.descriptor.hook_support))
                .field("used_fallback", json!(resolved.used_fallback))
                .field("skipped", json!(skipped_json)))
        }
        Err(reason) => Ok(CmdOutput::policy("unavailable", reason.clone())
            .field("primary", json!(primary))
            .field("fallbacks", json!(fallback))
            .field("selected", Value::Null)
            .field("error", json!(reason))),
    }
}

fn run_roles(ctx: &Ctx) -> CmdResult {
    let root = ctx.repo_root().unwrap_or_else(|_| ctx.cwd.clone());
    let roles = load_roles(&root).map_err(CmdError::new)?;

    let mut lines = Vec::with_capacity(roles.len() + 1);
    lines.push(format!(
        "{:<16} {:<12} {:<9} {:<22} {}",
        "ROLE", "EXECUTOR", "READ_ONLY", "FALLBACKS", "DESCRIPTION"
    ));
    for role in &roles {
        let fallbacks = if role.fallback_executors.is_empty() {
            "-".to_string()
        } else {
            role.fallback_executors.join(",")
        };
        lines.push(format!(
            "{:<16} {:<12} {:<9} {:<22} {}",
            role.name,
            role.executor,
            if role.read_only { "yes" } else { "no" },
            fallbacks,
            role.description
        ));
    }

    let roles_json = serde_json::to_value(&roles)
        .map_err(|err| CmdError::new(format!("serialize roles: {err}")))?;

    Ok(CmdOutput::ok("ok", lines.join("\n"))
        .field("count", json!(roles.len()))
        .field("repo_root", json!(root.to_string_lossy().into_owned()))
        .field("roles", roles_json))
}

fn run_mcp(ctx: &Ctx) -> CmdResult {
    let root = ctx.repo_root().unwrap_or_else(|_| ctx.cwd.clone());
    let report = match inspect_mcp_servers(&root) {
        Ok(report) => report,
        Err(reason) => {
            return Ok(
                CmdOutput::policy("BLOCKED", format!("mcp audit failed: {reason}"))
                    .field("present", json!(true))
                    .field("repo_root", json!(root.to_string_lossy().into_owned()))
                    .field("warnings_count", json!(0))
                    .field("servers", json!([]))
                    .field("error", json!(reason)),
            );
        }
    };

    let path_str = report.path.to_string_lossy().into_owned();
    if !report.present {
        return Ok(
            CmdOutput::ok("ok", format!("no .mcp.json found at {path_str}"))
                .field("present", json!(false))
                .field("path", json!(path_str))
                .field("warnings_count", json!(0))
                .field("servers", json!([])),
        );
    }

    let warnings_count = report.total_warnings();
    let mut lines = Vec::new();
    if report.servers.is_empty() {
        lines.push(format!("{path_str}: 0 MCP servers configured"));
    } else {
        lines.push(format!(
            "{:<20} {:<8} {:<32} {}",
            "SERVER", "MODE", "TARGET", "WARNINGS"
        ));
        for server in &report.servers {
            let target = if server.command_or_url.is_empty() {
                "-"
            } else {
                server.command_or_url.as_str()
            };
            lines.push(format!(
                "{:<20} {:<8} {:<32} {}",
                server.name,
                server.transport.as_str(),
                target,
                server.inline_secret_warnings.len()
            ));
            for warning in &server.inline_secret_warnings {
                lines.push(format!("  warning [{}]: {warning}", server.name));
            }
        }
    }
    lines.push(format!(
        "mcp audit: {} server(s), {warnings_count} inline secret warning(s)",
        report.servers.len()
    ));

    let servers_json = serde_json::to_value(&report.servers)
        .map_err(|err| CmdError::new(format!("serialize mcp servers: {err}")))?;
    let out = if report.has_warnings() {
        CmdOutput::policy("BLOCKED", lines.join("\n"))
    } else {
        CmdOutput::ok("ok", lines.join("\n"))
    };

    Ok(out
        .field("present", json!(true))
        .field("path", json!(path_str))
        .field("warnings_count", json!(warnings_count))
        .field("servers", servers_json))
}
