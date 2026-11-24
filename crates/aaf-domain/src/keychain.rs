use serde::{Deserialize, Serialize};

/// Controls whether macOS Seatbelt closes Keychain read/IPC access for sandboxed agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeychainPolicy {
    /// Close Keychain automatically for runs that do not rely on macOS OAuth Keychain
    /// (e.g. non-Claude executors or API-key authenticated runs); keep open for Claude OAuth.
    #[default]
    Auto,
    /// Always deny `~/Library/Keychains`, `com.apple.SecurityServer`, and `com.apple.securityd.xpc`.
    Always,
    /// Keep legacy macOS Keychain behavior (open in Seatbelt, advisory warn in `aaf doctor`).
    Never,
}
