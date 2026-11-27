//! Confined account and state-directory paths for [`super::GenericRunner`].

use std::path::Path;

use aaf_domain::account::ProviderKind;
use aaf_domain::ports::AgentRequest;

use crate::executors::AccountPaths;
use crate::executors::agy;

/// Entries of a Claude config dir closed for writing (matches `ClaudeCodeRunner::CONFIG_CLOSED`).
const CLAUDE_CONFIG_CLOSED: [&str; 3] = ["settings.json", "settings.local.json", "hooks"];

/// Entries of a Codex config directory that later runs execute or obey.
const CODEX_CONFIG_CLOSED: [&str; 7] = [
    "config.toml",
    "hooks.json",
    "AGENTS.md",
    "instructions.md",
    "rules",
    "skills",
    "plugins",
];

/// Extra Gemini / Antigravity config entries closed on top of [`agy::GEMINI_CONFIG_CLOSED`].
const GEMINI_EXTRA_CLOSED: [&str; 14] = [
    "settings.local.json",
    "hooks",
    "extensions",
    "policies",
    "commands",
    "antigravity",
    "antigravity-cli/bin",
    "antigravity-cli/mcp",
    "antigravity-cli/settings.json",
    "antigravity-ide/bin",
    "config/hooks.json",
    "config/mcp_config.json",
    "config/plugins",
    "config/node_modules",
];

/// Entries of an OpenCode config/state directory that later runs execute or obey.
const OPENCODE_CONFIG_CLOSED: [&str; 13] = [
    "opencode.json",
    "opencode.jsonc",
    "config.json",
    "plugin",
    "plugins",
    "tool",
    "tools",
    "agent",
    "agents",
    "command",
    "commands",
    "skill",
    "skills",
];

/// Entries of a Grok CLI config directory (`~/.grok`) that hold credentials,
/// hooks, rules, plugins, IPC sockets, caches, or executables (including
/// `downloads`, the symlink target of `bin/grok`) and must stay non-writable.
const GROK_CONFIG_CLOSED: [&str; 29] = [
    "auth.json",
    "auth.json.lock",
    "auth.json.commit.lock",
    "config.toml",
    "hooks",
    "hooks-paths",
    "rules",
    "skills",
    "agents",
    "installed-plugins",
    "marketplace-cache",
    "sandbox.toml",
    "trusted_folders.toml",
    "trusted_folders.toml.lock",
    "managed_config.lock",
    "settings_cache.json",
    "experiments_remote.json",
    "memory-v2",
    "completions",
    "downloads",
    "bin",
    "bundled",
    "bundles",
    "vendor",
    "worktrees",
    "worktrees.db",
    "grove",
    "dashboard",
    "leader.sock",
];

/// Minimal runtime subdirectories inside `~/.grok` opened writable while the
/// `~/.grok` root itself stays read-only (prevents bwrap creation of missing
/// config/hook dirs and prevents overwriting `downloads/` or planting sockets).
const GROK_RUNTIME_WRITABLE_SUBDIRS: [&str; 5] = [
    "sessions",
    "logs",
    "memtrace",
    "process-records",
    "long-running-background-tasks",
];

fn escape_regex_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for ch in s.chars() {
        if matches!(
            ch,
            '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$'
        ) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

fn grok_account_paths(config_dir: &str, sandbox_enabled: bool) -> AccountPaths {
    let mut writable = Vec::with_capacity(GROK_RUNTIME_WRITABLE_SUBDIRS.len());
    for sub in GROK_RUNTIME_WRITABLE_SUBDIRS {
        let dir = Path::new(config_dir).join(sub);
        if sandbox_enabled {
            let _ = std::fs::create_dir_all(&dir);
        }
        writable.push(dir.to_string_lossy().into_owned());
    }
    let root_esc = escape_regex_literal(config_dir.trim_end_matches('/'));
    let writable_regex = vec![
        format!(r#"^{root_esc}/active_sessions\.(json|lock)$"#),
        format!(r#"^{root_esc}/models_cache\.json(\.lock)?$"#),
    ];
    AccountPaths {
        writable,
        write_denied: denied_under(config_dir, &GROK_CONFIG_CLOSED),
        writable_regex,
    }
}

fn denied_under(root: &str, entries: &[&str]) -> Vec<String> {
    let base = root.trim_end_matches('/');
    entries
        .iter()
        .map(|name| format!("{base}/{name}"))
        .collect()
}

fn closed_dir_paths(config_dir: &str, entries: &[&str]) -> AccountPaths {
    AccountPaths {
        writable: vec![config_dir.to_string()],
        write_denied: denied_under(config_dir, entries),
        writable_regex: Vec::new(),
    }
}

fn gemini_account_paths(config_dir: &str, sandbox_enabled: bool) -> AccountPaths {
    let mut writable = Vec::with_capacity(2);
    for sub in ["tmp", "history"] {
        let dir = Path::new(config_dir).join(sub);
        if sandbox_enabled {
            let _ = std::fs::create_dir_all(&dir);
        }
        writable.push(dir.to_string_lossy().into_owned());
    }
    let mut write_denied = agy::closed_entries(config_dir);
    for extra in denied_under(config_dir, &GEMINI_EXTRA_CLOSED) {
        if !write_denied.contains(&extra) {
            write_denied.push(extra);
        }
    }
    AccountPaths {
        writable,
        write_denied,
        writable_regex: Vec::new(),
    }
}

/// Builds confined account paths for a generic CLI run from `request.account`,
/// or falls back to default home state dirs based on `program_name` when `None`.
pub fn account_paths_for(
    request: &AgentRequest,
    program_name: &str,
    home: Option<&str>,
) -> AccountPaths {
    if let Some(account) = &request.account {
        let dir = &account.config_dir;
        return match account.provider {
            ProviderKind::ClaudeCode => closed_dir_paths(dir, &CLAUDE_CONFIG_CLOSED),
            ProviderKind::Codex => closed_dir_paths(dir, &CODEX_CONFIG_CLOSED),
            ProviderKind::Antigravity => gemini_account_paths(dir, request.sandbox.enabled),
            ProviderKind::OpenCode => closed_dir_paths(dir, &OPENCODE_CONFIG_CLOSED),
            ProviderKind::Grok => grok_account_paths(dir, request.sandbox.enabled),
        };
    }
    let Some(home) = home
        .filter(|h| !h.is_empty())
        .map(|h| h.trim_end_matches('/'))
    else {
        return AccountPaths::default();
    };
    let at = |sub: &str| format!("{home}/{sub}");
    match program_name {
        "claude" => AccountPaths {
            writable: vec![at(".claude"), at(".claude.json")],
            write_denied: denied_under(&at(".claude"), &CLAUDE_CONFIG_CLOSED),
            writable_regex: Vec::new(),
        },
        "codex" => closed_dir_paths(&at(".codex"), &CODEX_CONFIG_CLOSED),
        "gemini" | "agy" | "antigravity" => {
            gemini_account_paths(&at(".gemini"), request.sandbox.enabled)
        }
        "opencode" => closed_dir_paths(&at(".local/share/opencode"), &OPENCODE_CONFIG_CLOSED),
        "grok" => grok_account_paths(&at(".grok"), request.sandbox.enabled),
        _ => AccountPaths::default(),
    }
}

/// Builds the confined account paths for a generic CLI run with an explicit account.
pub fn account_paths(request: &AgentRequest) -> AccountPaths {
    account_paths_for(request, "", None)
}
