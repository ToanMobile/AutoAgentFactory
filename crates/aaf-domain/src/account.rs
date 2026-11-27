//! Agent accounts (one config directory = one account).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    ClaudeCode,
    Codex,
    Antigravity,
    OpenCode,
    Grok,
}

impl ProviderKind {
    pub const ALL: [ProviderKind; 5] = [
        ProviderKind::ClaudeCode,
        ProviderKind::Codex,
        ProviderKind::Antigravity,
        ProviderKind::OpenCode,
        ProviderKind::Grok,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ProviderKind::ClaudeCode => "claude_code",
            ProviderKind::Codex => "codex",
            ProviderKind::Antigravity => "antigravity",
            ProviderKind::OpenCode => "open_code",
            ProviderKind::Grok => "grok",
        }
    }

    pub fn parse(s: &str) -> Option<ProviderKind> {
        ProviderKind::ALL.into_iter().find(|p| p.as_str() == s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AccountProfile {
    /// Stable id derived from provider + config dir (safe as a file name).
    pub id: String,
    pub provider: ProviderKind,
    pub config_dir: String,
    /// Human-facing label; never part of the id. Empty in records written
    /// before the field existed.
    #[serde(default)]
    pub name: String,
}

/// Display name used when no alias is configured: the last component of the
/// config dir without its leading `.claude-` or `.` (`~/.claude-work` is
/// `work`, `~/.codex` is `codex`). The component is kept whole when
/// stripping would leave nothing.
pub fn default_account_name(config_dir: &str) -> String {
    let trimmed = config_dir.trim_end_matches('/');
    if trimmed.is_empty() {
        return config_dir.to_string();
    }
    let component = trimmed.rsplit('/').next().unwrap_or(trimmed);
    let stripped = component
        .strip_prefix(".claude-")
        .or_else(|| component.strip_prefix('.'))
        .unwrap_or(component);
    if stripped.is_empty() {
        component.to_string()
    } else {
        stripped.to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AccountStatus {
    Available,
    Cooldown { until: i64 },
    Leased,
}

/// Persisted account state plus usage derived from the `runs` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountState {
    pub profile: AccountProfile,
    pub cooldown_until: Option<i64>,
}

impl AccountState {
    pub fn in_cooldown(&self, now: i64) -> bool {
        self.cooldown_until.is_some_and(|until| until > now)
    }
}
