//! Reusable agent Role profiles and read-only MCP server registry audit.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::executors::registry::{find_descriptor, validate_flag_value};

mod mcp_audit;
pub use mcp_audit::{McpAuditReport, McpServerAudit, McpTransport, inspect_mcp_servers};

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

#[cfg(test)]
mod tests;
