//! Reusable agent Role profiles and read-only MCP server registry audit.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::executors::registry::{find_descriptor, validate_flag_value};
use crate::executors::secret_mask::mask_named_and_patterns;

/// Repo-relative directory for custom TOML role overlays.
pub const ROLES_DIR_REL: &str = ".autofactory/roles";

/// Repo-relative path of the MCP server registry file.
pub const MCP_CONFIG_REL: &str = ".mcp.json";

/// Maximum accepted size of a single role TOML file (256 KiB).
pub const MAX_ROLE_FILE_BYTES: usize = 256 * 1024;

/// Maximum accepted length of a role `system_prompt` in Unicode scalar values.
pub const MAX_SYSTEM_PROMPT_CHARS: usize = 8_000;

/// Maximum accepted size of `.mcp.json` (1 MiB).
pub const MAX_MCP_JSON_BYTES: usize = 1024 * 1024;

/// Reusable agent role profile specifying executor routing, flags, skills, and prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleProfile {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub executor: String,
    #[serde(default)]
    pub fallback_executors: Vec<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub system_prompt: String,
}

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

/// Returns the 4 built-in starter role profiles (`architect`, `implementer`,
/// `reviewer`, `qa-tester`).
pub fn builtin_roles() -> Vec<RoleProfile> {
    let mk = |name: &str,
              desc: &str,
              exec: &str,
              fbs: &[&str],
              effort: Option<&str>,
              ro: bool,
              skills: &[&str],
              prompt: &str| RoleProfile {
        name: name.into(),
        description: desc.into(),
        executor: exec.into(),
        fallback_executors: fbs.iter().map(|s| (*s).into()).collect(),
        model: None,
        effort: effort.map(Into::into),
        read_only: ro,
        skills: skills.iter().map(|s| (*s).into()).collect(),
        system_prompt: prompt.into(),
    };
    vec![
        mk(
            "architect",
            "Read-only architecture analysis, risk review, and implementation planning",
            "claude",
            &["codex", "gemini"],
            Some("high"),
            true,
            &["planning"],
            "Inspect the codebase read-only and produce a minimal, verifiable plan with clear acceptance criteria.",
        ),
        mk(
            "implementer",
            "Focused feature implementation, refactoring, and deterministic unit tests",
            "claude",
            &["codex", "opencode"],
            None,
            false,
            &["testing"],
            "Implement the assigned scope cleanly, keep diffs minimal, and add deterministic unit tests for every invariant.",
        ),
        mk(
            "reviewer",
            "Read-only security, correctness, and regression code review",
            "codex",
            &["claude", "gemini"],
            None,
            true,
            &["code-review", "security"],
            "Review the diff strictly read-only, prioritizing security boundaries, edge cases, and test coverage gaps.",
        ),
        mk(
            "qa-tester",
            "Verification, adversarial edge-case testing, and regression checks",
            "claude",
            &["gemini", "codex"],
            None,
            false,
            &["qa", "testing"],
            "Verify acceptance criteria with deterministic tests covering both allow and deny paths.",
        ),
    ]
}

fn read_regular_bounded(path: &Path, max_bytes: usize) -> Result<String, String> {
    let meta = fs::symlink_metadata(path)
        .map_err(|err| format!("cannot stat {}: {err}", path.display()))?;
    if meta.file_type().is_symlink() {
        return Err(format!("refusing symlink {}", path.display()));
    }
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if meta.len() > max_bytes as u64 {
        return Err(format!(
            "{} exceeds maximum allowed size of {max_bytes} bytes",
            path.display()
        ));
    }
    let file = File::open(path).map_err(|err| format!("cannot open {}: {err}", path.display()))?;
    let mut buf = Vec::new();
    let limit = u64::try_from(max_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    file.take(limit)
        .read_to_end(&mut buf)
        .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    if buf.len() > max_bytes {
        return Err(format!(
            "{} exceeds maximum allowed size of {max_bytes} bytes",
            path.display()
        ));
    }
    String::from_utf8(buf).map_err(|err| format!("{} is not valid UTF-8: {err}", path.display()))
}

fn ensure_not_symlink_dir(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(format!("refusing symlink {}", path.display()));
            }
            if !meta.is_dir() {
                return Err(format!("{} is not a directory", path.display()));
            }
            Ok(true)
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(format!("cannot stat {}: {err}", path.display())),
    }
}

fn validate_executor_name(raw: &str, field: &str, role_name: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("custom") {
        return Ok("custom".to_string());
    }
    if let Some(desc) = find_descriptor(trimmed) {
        return Ok(desc.id.to_string());
    }
    Err(format!(
        "role {role_name:?} has unknown {field} {raw:?}; expected a built-in executor id or \"custom\""
    ))
}

fn is_valid_skill_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn validate_and_normalize_role(
    mut role: RoleProfile,
    source: &Path,
) -> Result<RoleProfile, String> {
    let name = role.name.trim();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(format!(
            "{}: invalid role name {:?} (use ASCII letters, digits, '-', '_', '.')",
            source.display(),
            role.name
        ));
    }
    role.name = name.to_string();
    role.executor = validate_executor_name(&role.executor, "executor", &role.name)?;
    role.fallback_executors = role
        .fallback_executors
        .iter()
        .map(|fb| validate_executor_name(fb, "fallback_executors", &role.name))
        .collect::<Result<_, _>>()?;
    if let Some(model) = &role.model {
        validate_flag_value("model", model)
            .map_err(|err| format!("{}: {err}", source.display()))?;
    }
    if let Some(effort) = &role.effort {
        validate_flag_value("effort", effort)
            .map_err(|err| format!("{}: {err}", source.display()))?;
    }
    if role.system_prompt.chars().count() > MAX_SYSTEM_PROMPT_CHARS {
        return Err(format!(
            "{}: system_prompt exceeds maximum of {MAX_SYSTEM_PROMPT_CHARS} chars",
            source.display()
        ));
    }
    role.skills = role
        .skills
        .iter()
        .map(|raw| {
            let slug = raw.trim();
            if !is_valid_skill_slug(slug) {
                return Err(format!(
                    "{}: invalid skill slug {:?} (use ASCII [A-Za-z0-9][A-Za-z0-9_-]*, <= 64 chars)",
                    source.display(),
                    raw
                ));
            }
            Ok(slug.to_string())
        })
        .collect::<Result<_, _>>()?;
    Ok(role)
}

/// Loads the 4 built-in starter roles and overlays/adds repo roles from
/// `<repo_root>/.autofactory/roles/*.toml`.
///
/// Rejects symlinks, unknown TOML fields, invalid flag values, and unknown
/// executor names (anything not in the built-in registry and not `"custom"`).
pub fn load_roles(repo_root: &Path) -> Result<Vec<RoleProfile>, String> {
    let mut roles = builtin_roles();

    let autofactory_dir = repo_root.join(".autofactory");
    if !ensure_not_symlink_dir(&autofactory_dir)? {
        return Ok(roles);
    }
    let roles_dir = autofactory_dir.join("roles");
    if !ensure_not_symlink_dir(&roles_dir)? {
        return Ok(roles);
    }

    let mut toml_files: Vec<PathBuf> = Vec::new();
    let entries = fs::read_dir(&roles_dir)
        .map_err(|err| format!("cannot read {}: {err}", roles_dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("cannot read dir entry: {err}"))?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "toml") {
            toml_files.push(path);
        }
    }
    toml_files.sort();

    let mut seen_custom: HashSet<String> = HashSet::new();
    let mut extras: Vec<RoleProfile> = Vec::new();

    for path in toml_files {
        let text = read_regular_bounded(&path, MAX_ROLE_FILE_BYTES)?;
        let parsed: RoleProfile = toml::from_str(&text)
            .map_err(|err| format!("invalid role file {}: {err}", path.display()))?;
        let role = validate_and_normalize_role(parsed, &path)?;
        if !seen_custom.insert(role.name.clone()) {
            return Err(format!(
                "duplicate role {:?} declared in {}",
                role.name,
                path.display()
            ));
        }
        if let Some(existing) = roles.iter_mut().find(|item| item.name == role.name) {
            *existing = role;
        } else {
            extras.push(role);
        }
    }

    extras.sort_by(|a, b| a.name.cmp(&b.name));
    roles.extend(extras);
    Ok(roles)
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

#[cfg(test)]
mod tests;
