//! Advisory checks for `gate.head_lock_globs` coverage on existing
//! repositories whose `.autofactory/config.toml` predates newly added defaults.

use aaf_domain::config::{AGENT_CONFIG_GLOBS, RUNNER_CONFIG_GLOBS};

/// A list in `.autofactory/config.toml` replaces the defaults, so a config
/// written before the agent-file globs existed does not have them. The list
/// is the repository's own and is kept; this says what it leaves open.
pub(super) fn agent_files_check(globs: &[String]) -> Result<String, String> {
    let missing: Vec<&str> = AGENT_CONFIG_GLOBS
        .iter()
        .copied()
        .filter(|glob| !globs.iter().any(|have| have == glob))
        .collect();
    if missing.is_empty() {
        return Ok(format!(
            "gate.head_lock_globs locks {}",
            AGENT_CONFIG_GLOBS.join(", ")
        ));
    }
    Err(format!(
        "gate.head_lock_globs does not lock {}: a task can change what Claude Code loads from \
         the repository (instructions, hooks, MCP servers) without an audit; add them to the \
         list in .autofactory/config.toml",
        missing.join(", ")
    ))
}

/// Critical runner/build/lint globs that an older `.autofactory/config.toml`
/// may omit, leaving a task free to alter how tests or builds execute.
pub(super) fn runner_config_check(globs: &[String]) -> Result<String, String> {
    let missing: Vec<&str> = RUNNER_CONFIG_GLOBS
        .iter()
        .copied()
        .filter(|glob| !globs.iter().any(|have| have == glob))
        .collect();
    if missing.is_empty() {
        return Ok(format!(
            "gate.head_lock_globs locks all {} default runner/build config globs",
            RUNNER_CONFIG_GLOBS.len()
        ));
    }
    Err(format!(
        "gate.head_lock_globs is missing {} runner/build config glob(s) ({}): a task can alter \
         test runners or build configs without an audit; add them to .autofactory/config.toml",
        missing.len(),
        missing.join(", ")
    ))
}
