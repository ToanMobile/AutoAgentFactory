//! Read-only audit of a repository's MCP server registry (`.mcp.json`).

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{MAX_MCP_JSON_BYTES, MCP_CONFIG_REL, read_regular_bounded};
use crate::executors::secret_mask::mask_named_and_patterns;

/// Transport mode of a server entry in `.mcp.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    Stdio,
    Http,
    Sse,
    Unknown,
}

impl McpTransport {
    /// Short lowercase transport label.
    pub fn as_str(self) -> &'static str {
        match self {
            McpTransport::Stdio => "stdio",
            McpTransport::Http => "http",
            McpTransport::Sse => "sse",
            McpTransport::Unknown => "unknown",
        }
    }
}

/// Read-only audit summary for one server declared under `mcpServers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerAudit {
    pub name: String,
    pub transport: McpTransport,
    pub command_or_url: String,
    pub env_keys: Vec<String>,
    pub inline_secret_warnings: Vec<String>,
}

/// Full read-only audit report for a repository's `.mcp.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpAuditReport {
    pub present: bool,
    pub path: PathBuf,
    pub servers: Vec<McpServerAudit>,
}

impl McpAuditReport {
    /// Total count of inline secret warnings across all audited MCP servers.
    pub fn total_warnings(&self) -> usize {
        self.servers
            .iter()
            .map(|server| server.inline_secret_warnings.len())
            .sum()
    }

    /// True when any audited MCP server triggered an inline secret warning.
    pub fn has_warnings(&self) -> bool {
        self.total_warnings() > 0
    }
}

const CREDENTIAL_SUBSTRINGS: &[&str] = &[
    "_TOKEN",
    "_KEY",
    "_SECRET",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    "PRIVATE_KEY",
    "DATABASE_URL",
    "AUTH_HEADER",
    "BEARER",
    "COOKIE",
    "API_KEY",
    "APIKEY",
    "AUTHORIZATION",
];

fn is_credential_key(key: &str) -> bool {
    let normalized = key.trim().to_ascii_uppercase().replace('-', "_");
    let benign = matches!(
        normalized.as_str(),
        "SSH_AUTH_SOCK"
            | "COLORTERM"
            | "TERM_SESSION_ID"
            | "KEYMAP"
            | "CONTENT_TYPE"
            | "USER_AGENT"
            | "ACCEPT"
    ) || normalized.contains("PUBLIC_KEY")
        || normalized.contains("KEYMAP");
    if benign {
        return false;
    }
    matches!(
        normalized.as_str(),
        "TOKEN"
            | "SECRET"
            | "PASSWORD"
            | "PASSWD"
            | "API_KEY"
            | "APIKEY"
            | "AUTHORIZATION"
            | "AUTH"
            | "COOKIE"
            | "CREDENTIAL"
            | "CREDENTIALS"
            | "DATABASE_URL"
    ) || CREDENTIAL_SUBSTRINGS.iter().any(|s| normalized.contains(s))
}

fn is_env_ident(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn is_single_env_placeholder(raw: &str) -> bool {
    if let Some(inner) = raw.strip_prefix("${").and_then(|s| s.strip_suffix('}')) {
        let ident = inner.strip_prefix("env:").unwrap_or(inner);
        return is_env_ident(ident);
    }
    if let Some(ident) = raw.strip_prefix('$') {
        return is_env_ident(ident);
    }
    false
}

fn is_placeholder_value(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() || is_single_env_placeholder(trimmed) {
        return true;
    }
    if let Some((scheme, rest)) = trimmed.split_once(|c: char| c.is_ascii_whitespace()) {
        let rest = rest.trim();
        if matches!(
            scheme.to_ascii_lowercase().as_str(),
            "bearer" | "basic" | "token"
        ) && is_single_env_placeholder(rest)
        {
            return true;
        }
    }
    false
}

fn check_secret_field(scope: &str, key: &str, val: &str, warnings: &mut Vec<String>) {
    if is_placeholder_value(val) {
        return;
    }
    let masked = mask_named_and_patterns(val, &[], &[]);
    if masked != val {
        warnings.push(format!(
            "{scope}.{key} contains a literal secret pattern (replace with ${{ENV_VAR}})"
        ));
    } else if is_credential_key(key) {
        warnings.push(format!(
            "{scope}.{key} sets a literal value on a credential key (replace with ${{ENV_VAR}})"
        ));
    }
}

fn audit_one_server(name: &str, cfg: &Value) -> Result<McpServerAudit, String> {
    let Some(obj) = cfg.as_object() else {
        return Err(format!(
            "{MCP_CONFIG_REL}: server {name:?} must be a JSON object"
        ));
    };

    let explicit_type = obj
        .get("type")
        .or_else(|| obj.get("transport"))
        .and_then(Value::as_str)
        .map(|s| s.trim().to_ascii_lowercase());
    let command = obj.get("command").and_then(Value::as_str);
    let url = obj.get("url").and_then(Value::as_str);

    let (transport, raw_target) = match explicit_type.as_deref() {
        Some("stdio") => (McpTransport::Stdio, command.unwrap_or("")),
        Some("sse") => (McpTransport::Sse, url.unwrap_or("")),
        Some("http" | "streamable-http" | "streamable_http") => {
            (McpTransport::Http, url.unwrap_or(""))
        }
        _ => {
            if let Some(cmd) = command {
                (McpTransport::Stdio, cmd)
            } else if let Some(u) = url {
                let kind = if u.trim_end_matches('/').ends_with("/sse") {
                    McpTransport::Sse
                } else {
                    McpTransport::Http
                };
                (kind, u)
            } else {
                (McpTransport::Unknown, "")
            }
        }
    };

    let mut warnings = Vec::new();
    let redacted_target = mask_named_and_patterns(raw_target, &[], &[]);
    if redacted_target != raw_target {
        warnings.push(
            "command_or_url contains embedded credentials (move secrets to env references)"
                .to_string(),
        );
    }

    if let Some(args) = obj.get("args").and_then(Value::as_array) {
        for (idx, item) in args.iter().enumerate() {
            if let Some(arg) = item.as_str()
                && !is_placeholder_value(arg)
                && mask_named_and_patterns(arg, &[], &[]) != arg
            {
                warnings.push(format!(
                    "args[{idx}] contains a literal secret pattern (replace with ${{ENV_VAR}})"
                ));
            }
        }
    }

    let mut env_keys = Vec::new();
    if let Some(env_obj) = obj.get("env").and_then(Value::as_object) {
        let sorted: BTreeMap<&String, &Value> = env_obj.iter().collect();
        for (key, value) in sorted {
            env_keys.push(key.clone());
            if let Some(text) = value.as_str() {
                check_secret_field("env", key, text, &mut warnings);
            }
        }
    }

    if let Some(headers_obj) = obj.get("headers").and_then(Value::as_object) {
        let sorted: BTreeMap<&String, &Value> = headers_obj.iter().collect();
        for (key, value) in sorted {
            if let Some(text) = value.as_str() {
                check_secret_field("headers", key, text, &mut warnings);
            }
        }
    }

    Ok(McpServerAudit {
        name: name.to_string(),
        transport,
        command_or_url: redacted_target,
        env_keys,
        inline_secret_warnings: warnings,
    })
}

/// Performs a strictly read-only audit of `<repo_root>/.mcp.json`.
///
/// Refuses symlinks, enforces a 1 MiB file size bound, never spawns or connects
/// to any MCP server, redacts credentials embedded in URLs, and flags inline
/// secret leaks without ever echoing raw secret values.
pub fn inspect_mcp_servers(repo_root: &Path) -> Result<McpAuditReport, String> {
    let path = repo_root.join(MCP_CONFIG_REL);
    match fs::symlink_metadata(&path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(McpAuditReport {
                present: false,
                path,
                servers: Vec::new(),
            });
        }
        Err(err) => return Err(format!("cannot stat {}: {err}", path.display())),
        Ok(_) => {}
    }

    let text = read_regular_bounded(&path, MAX_MCP_JSON_BYTES)?;
    let root: Value =
        serde_json::from_str(&text).map_err(|err| format!("malformed {MCP_CONFIG_REL}: {err}"))?;
    let Some(root_obj) = root.as_object() else {
        return Err(format!(
            "{MCP_CONFIG_REL}: top-level value must be a JSON object"
        ));
    };

    let Some(servers_val) = root_obj.get("mcpServers") else {
        return Ok(McpAuditReport {
            present: true,
            path,
            servers: Vec::new(),
        });
    };
    let Some(servers_obj) = servers_val.as_object() else {
        return Err(format!(
            "{MCP_CONFIG_REL}: \"mcpServers\" must be a JSON object"
        ));
    };

    let sorted: BTreeMap<&String, &Value> = servers_obj.iter().collect();
    let mut servers = Vec::with_capacity(sorted.len());
    for (name, cfg) in sorted {
        servers.push(audit_one_server(name, cfg)?);
    }

    Ok(McpAuditReport {
        present: true,
        path,
        servers,
    })
}
