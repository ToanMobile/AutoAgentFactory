//! Lightweight, read-only `gate.base_ref` reader for `aaf-hook`.
//!
//! `aaf-hook` runs on every tool call without pulling in the `toml` crate or
//! spawning `git`. This reader scans `[gate]` in `<repo_root>/.autofactory/config.toml`
//! (capped at 64 KiB) to extract a configured `base_ref` branch name so
//! `git_guard`, `git_persist`, and `main_branch_guard` protect custom base
//! branches (for example `trunk` or `staging`) alongside `main`/`master`/`release/*`.

use std::fs;
use std::path::Path;

use crate::Ctx;

/// Hard upper bound on `.autofactory/config.toml` read by the hook fast path.
const MAX_CONFIG_BYTES: u64 = 64 * 1024;

/// Parses `[gate] base_ref = "..."` from a TOML snippet without a TOML crate.
pub(crate) fn parse_gate_base_ref(toml_text: &str) -> Option<String> {
    let mut in_gate = false;
    for raw_line in toml_text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(after_open) = line.strip_prefix('[') {
            let header = after_open
                .split(']')
                .next()
                .map(str::trim)
                .unwrap_or_default();
            in_gate = header == "gate";
            continue;
        }
        if !in_gate {
            continue;
        }
        let Some((key, raw_val)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "base_ref" {
            continue;
        }
        return extract_branch_value(raw_val);
    }
    None
}

fn extract_branch_value(raw_val: &str) -> Option<String> {
    let trimmed = raw_val.trim();
    let value = if let Some(rest) = trimmed.strip_prefix('"') {
        let end = rest.find('"')?;
        &rest[..end]
    } else if let Some(rest) = trimmed.strip_prefix('\'') {
        let end = rest.find('\'')?;
        &rest[..end]
    } else {
        trimmed.split('#').next()?.trim()
    };
    normalize_base_ref(value)
}

fn normalize_base_ref(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let stripped = trimmed
        .strip_prefix("refs/heads/")
        .or_else(|| trimmed.strip_prefix("refs/remotes/origin/"))
        .or_else(|| trimmed.strip_prefix("origin/"))
        .unwrap_or(trimmed);
    if stripped.is_empty()
        || stripped.eq_ignore_ascii_case("auto")
        || stripped.eq_ignore_ascii_case("head")
        || stripped.contains(char::is_whitespace)
    {
        return None;
    }
    Some(stripped.to_string())
}

/// Reads `[gate].base_ref` from `<repo_root>/.autofactory/config.toml` if present.
pub(crate) fn configured_base_ref(repo_root: &Path) -> Option<String> {
    let path = repo_root.join(".autofactory/config.toml");
    let meta = fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > MAX_CONFIG_BYTES {
        return None;
    }
    let content = fs::read_to_string(&path).ok()?;
    parse_gate_base_ref(&content)
}

/// Resolves the configured `gate.base_ref` for the repository containing `dir`.
pub(crate) fn for_dir(dir: &Path, ctx: &Ctx) -> Option<String> {
    let root = (ctx.env.repo_root)(dir).or_else(|| ctx.root())?;
    configured_base_ref(&root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gate_base_ref_with_comments_and_other_sections() {
        let toml = r#"
            [project]
            profile = "rust"

            [gate]
            # comment
            base_ref = "origin/trunk" # inline comment

            [run]
            base_ref = "ignored"
        "#;
        assert_eq!(parse_gate_base_ref(toml).as_deref(), Some("trunk"));
    }

    #[test]
    fn ignores_auto_empty_and_non_gate_sections() {
        assert_eq!(parse_gate_base_ref("[gate]\nbase_ref = \"auto\"\n"), None);
        assert_eq!(parse_gate_base_ref("[gate]\nbase_ref = ''\n"), None);
        assert_eq!(parse_gate_base_ref("[run]\nbase_ref = \"trunk\"\n"), None);
        assert_eq!(
            parse_gate_base_ref("[gate]\nbase_ref = 'refs/heads/staging'\n").as_deref(),
            Some("staging")
        );
    }
}
