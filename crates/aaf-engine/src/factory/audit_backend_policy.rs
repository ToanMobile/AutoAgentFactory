//! Cross-audit provider resolution, independence policy, and command-template
//! construction helpers for [`crate::factory::audit_backend`].

use std::ffi::OsStr;
use std::path::Path;

use aaf_domain::config::CrossAuditConfig;

const PROMPT_PLACEHOLDER: &str = "{prompt}";

fn canonical_provider(provider: &str) -> String {
    match provider.trim().to_ascii_lowercase().as_str() {
        "antigravity" | "gemini" => "gemini".to_string(),
        other => other.to_string(),
    }
}

/// True when two provider names refer to the same underlying provider
/// (e.g. `gemini` and `antigravity`).
pub fn same_audit_provider(a: &str, b: &str) -> bool {
    canonical_provider(a) == canonical_provider(b)
}

/// True when `executor_cmd` invokes a `claude` binary as `argv[0]`.
pub fn audit_cmd_is_claude(executor_cmd: &str) -> bool {
    let Ok(words) = split_cmd_words(executor_cmd.trim()) else {
        return false;
    };
    let Some(arg0) = words.first() else {
        return false;
    };
    Path::new(arg0)
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("claude"))
}

/// Effective audit provider name from config (`[cross_audit].executor` when
/// non-empty, `"custom"` when only `executor_cmd` is set, `"claude"` when
/// `executor_cmd` invokes `claude`, otherwise `[run].executor`).
pub fn configured_audit_provider<'a>(
    run_executor: &'a str,
    cross_audit: &'a CrossAuditConfig,
) -> &'a str {
    let custom_cmd = cross_audit.executor_cmd.trim();
    if !custom_cmd.is_empty() && audit_cmd_is_claude(custom_cmd) {
        return "claude";
    }
    let trimmed = cross_audit.executor.trim();
    if !trimmed.is_empty() {
        trimmed
    } else if !custom_cmd.is_empty() {
        "custom"
    } else {
        run_executor.trim()
    }
}

/// Resolves `[cross_audit].executor = "auto"` once to another installed CLI
/// with a config directory under `home` (e.g. `gemini`/`codex` when
/// `run_executor == "claude"`), falling back to `run_executor` if none is ready.
pub fn resolve_auto_audit_executor(
    run_executor: &str,
    configured: &str,
    path_var: &OsStr,
    home: &Path,
) -> String {
    let trimmed = configured.trim();
    if trimmed.is_empty() {
        return run_executor.trim().to_string();
    }
    if !trimmed.eq_ignore_ascii_case("auto") {
        return trimmed.to_string();
    }
    let has_claude_dir = |h: &Path| {
        h.join(".claude").is_dir()
            || std::fs::read_dir(h).ok().is_some_and(|entries| {
                entries.flatten().any(|e| {
                    e.path().is_dir()
                        && e.file_name()
                            .to_str()
                            .is_some_and(|n| n.starts_with(".claude-"))
                })
            })
    };
    const CANDIDATES: [(&str, &[&str]); 4] = [
        ("gemini", &[".gemini"]),
        ("codex", &[".codex"]),
        ("claude", &[".claude"]),
        ("opencode", &[".local/share/opencode", ".config/opencode"]),
    ];
    for (candidate, dirs) in CANDIDATES {
        if same_audit_provider(run_executor, candidate) {
            continue;
        }
        let installed = crate::executors::registry::find_descriptor(candidate)
            .and_then(|d| crate::executors::registry::resolve_binary(d, path_var))
            .is_some();
        let configured_dir = if candidate == "claude" {
            has_claude_dir(home)
        } else {
            dirs.iter().any(|rel| home.join(rel).is_dir())
        };
        if installed && configured_dir {
            return candidate.to_string();
        }
    }
    run_executor.trim().to_string()
}

/// Computes `(reviewer, same_account)` for an audit approval and enforces
/// `require_different_provider` (fail-closed when the auditor shares the
/// author's provider).
pub fn resolve_audit_policy(
    run_provider: &str,
    author_account: Option<&str>,
    audit_provider: &str,
    auditor_account: Option<&str>,
    require_different_provider: bool,
) -> Result<(String, bool), String> {
    let run_p = run_provider.trim();
    let audit_p = audit_provider.trim();
    let run_canon = canonical_provider(run_p);
    let audit_canon = canonical_provider(audit_p);
    let different_provider = audit_canon != run_canon;
    if !different_provider && require_different_provider {
        return Err(format!(
            "cross_audit.require_different_provider is true, but auditor uses the same provider \
             ({audit_p}) as [run].executor ({run_p}); configure a different [cross_audit].executor"
        ));
    }
    let same_account = if audit_canon == "custom" {
        // Unverified custom commands may invoke any underlying account/CLI.
        true
    } else if different_provider {
        false
    } else {
        // Unknown on either side counts as the same account: never overclaim
        // independence within the same provider.
        match (author_account, auditor_account) {
            (Some(author), Some(auditor)) => author == auditor,
            _ => true,
        }
    };
    let reviewer = format!(
        "cross-audit:{audit_p}/{}",
        auditor_account.unwrap_or("default")
    );
    Ok((reviewer, same_account))
}

fn validate_cli_value(field: &str, value: &str) -> Result<(), String> {
    if value.starts_with('-') || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!(
            "invalid cross_audit.{field} {value:?}: must not start with '-' or contain whitespace or control chars"
        ));
    }
    Ok(())
}

/// Builds the `GenericRunner` argv template for non-Claude `[cross_audit]`
/// executors (`codex`, `gemini`, `antigravity`, `opencode`, `custom`) or an
/// explicit `executor_cmd` override.
pub fn build_audit_command(cfg: &CrossAuditConfig) -> Result<Vec<String>, String> {
    let custom = cfg.executor_cmd.trim();
    if !custom.is_empty() {
        let mut words = split_cmd_words(custom)?;
        if words.is_empty() {
            return Err("cross_audit.executor_cmd is empty".to_string());
        }
        let has_placeholder = words[1..]
            .iter()
            .any(|arg| arg.contains(PROMPT_PLACEHOLDER));
        if !has_placeholder {
            // `GenericRunner` passes the prompt as an argv element (no stdin):
            // replace a trailing `-` placeholder or append `{prompt}`.
            if words.len() > 1
                && let Some(last) = words.last_mut()
                && last == "-"
            {
                *last = PROMPT_PLACEHOLDER.to_string();
            } else {
                words.push(PROMPT_PLACEHOLDER.to_string());
            }
        }
        return Ok(words);
    }

    let exec = cfg.executor.trim().to_ascii_lowercase();
    let model = cfg.model.trim();
    let effort = cfg.effort.trim();
    if !model.is_empty() {
        validate_cli_value("model", model)?;
    }
    if !effort.is_empty() {
        validate_cli_value("effort", effort)?;
    }
    match exec.as_str() {
        "codex" => {
            let mut cmd = vec![
                "codex".into(),
                "exec".into(),
                "--sandbox".into(),
                "read-only".into(),
            ];
            if !model.is_empty() {
                cmd.extend(["-m".into(), model.into()]);
            }
            if !effort.is_empty() {
                cmd.extend(["-c".into(), format!("model_reasoning_effort={effort}")]);
            }
            cmd.push(PROMPT_PLACEHOLDER.into());
            Ok(cmd)
        }
        "gemini" | "antigravity" => {
            let mut cmd = vec!["gemini".into(), "--approval-mode".into(), "plan".into()];
            if !model.is_empty() {
                cmd.extend(["-m".into(), model.into()]);
            }
            cmd.extend(["-p".into(), PROMPT_PLACEHOLDER.into()]);
            Ok(cmd)
        }
        "opencode" => {
            let mut cmd = vec!["opencode".into(), "run".into()];
            if !model.is_empty() {
                cmd.extend(["-m".into(), model.into()]);
            }
            cmd.push(PROMPT_PLACEHOLDER.into());
            Ok(cmd)
        }
        "custom" => {
            Err("cross_audit.executor = \"custom\" requires cross_audit.executor_cmd".into())
        }
        other => Err(format!(
            "unknown cross_audit.executor {other:?}: expected claude, codex, gemini, antigravity, opencode or custom"
        )),
    }
}

/// Minimal quote-aware argv splitter (no shell expansion).
fn split_cmd_words(input: &str) -> Result<Vec<String>, String> {
    let (mut words, mut cur, mut in_word, mut quote) = (Vec::new(), String::new(), false, '\0');
    let mut chars = input.chars();
    let trail = || "trailing backslash in cross_audit.executor_cmd".to_string();
    while let Some(c) = chars.next() {
        match quote {
            '\'' if c == '\'' => quote = '\0',
            '\'' => cur.push(c),
            '"' if c == '"' => quote = '\0',
            '"' if c == '\\' => cur.push(chars.next().ok_or_else(trail)?),
            '"' => cur.push(c),
            _ => match c {
                '\'' | '"' => (quote, in_word) = (c, true),
                '\\' => {
                    cur.push(chars.next().ok_or_else(trail)?);
                    in_word = true;
                }
                ch if ch.is_whitespace() => {
                    if in_word {
                        words.push(std::mem::take(&mut cur));
                        in_word = false;
                    }
                }
                other => {
                    cur.push(other);
                    in_word = true;
                }
            },
        }
    }
    if quote != '\0' {
        return Err("unterminated quote in cross_audit.executor_cmd".to_string());
    }
    if in_word {
        words.push(cur);
    }
    Ok(words)
}
