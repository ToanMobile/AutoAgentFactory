//! Tier-1 Telegram remote-companion configuration (no I/O).

use serde::{Deserialize, Serialize};

/// Default environment variable carrying the Telegram Bot API token.
pub const DEFAULT_BOT_TOKEN_ENV: &str = "AAF_TELEGRAM_BOT_TOKEN";
/// Default Telegram Bot API base URL (overridable in tests).
pub const DEFAULT_API_BASE_URL: &str = "https://api.telegram.org";
/// Default long-poll timeout in seconds.
pub const DEFAULT_POLL_TIMEOUT_SECS: u64 = 25;
/// Maximum allowed Telegram long-poll timeout in seconds.
pub const MAX_POLL_TIMEOUT_SECS: u64 = 50;
/// Default confirmation nonce TTL in seconds (5 minutes).
pub const DEFAULT_CONFIRM_TTL_SECS: u64 = 300;

/// Tier-1 machine-wide configuration for `aaf telegram`.
///
/// Security invariants:
/// - Read ONLY from Tier 1 (`~/.config/autofactory/config.toml`), never from repo
///   or local config.
/// - `allowed_chat_ids` empty = fail-closed (no chat is authorized).
/// - `allowed_user_ids` empty = fail-closed for non-private chats (only direct
///   messages with `chat.type == "private"`, `chat.id > 0`, and `from.id == chat.id`
///   are authorized); when non-empty, `from.id` must be listed alongside `chat.id`.
/// - Mutating actions (`allow_approve`, `allow_mutating`) are opt-in (`false` by
///   default) and always require a single-use two-step `/confirm <code>`.
/// - Token is loaded from `bot_token_env` (never stored in TOML).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TelegramConfig {
    pub enabled: bool,
    pub bot_token_env: String,
    pub allowed_chat_ids: Vec<i64>,
    #[serde(default)]
    pub allowed_user_ids: Vec<i64>,
    pub poll_timeout_secs: u64,
    pub confirm_ttl_secs: u64,
    pub allow_approve: bool,
    pub allow_mutating: bool,
    pub api_base_url: String,
}

impl Default for TelegramConfig {
    fn default() -> Self {
        TelegramConfig {
            enabled: false,
            bot_token_env: DEFAULT_BOT_TOKEN_ENV.to_string(),
            allowed_chat_ids: Vec::new(),
            allowed_user_ids: Vec::new(),
            poll_timeout_secs: DEFAULT_POLL_TIMEOUT_SECS,
            confirm_ttl_secs: DEFAULT_CONFIRM_TTL_SECS,
            allow_approve: false,
            allow_mutating: false,
            api_base_url: DEFAULT_API_BASE_URL.to_string(),
        }
    }
}
