//! Enforces the `🤖 aaf-bot:` marker and blocks PR approvals on GitHub MCP tools.

use serde_json::Value;

use super::{BOT_MARKER, deny, has_bot_marker};
use crate::Denial;

/// True when `tool` is an MCP tool belonging to a GitHub server (`mcp__*github*`).
pub(crate) fn is_github_mcp_tool(tool: &str) -> bool {
    tool.starts_with("mcp__") && tool.to_ascii_lowercase().contains("github")
}

/// Checks a GitHub MCP tool call for missing bot markers or PR approval.
pub fn check_mcp(tool: &str, tool_input: Option<&Value>) -> Option<Denial> {
    if !is_github_mcp_tool(tool) {
        return None;
    }
    let action = tool.rsplit("__").next().unwrap_or("").to_ascii_lowercase();
    if action.starts_with("get_")
        || action.starts_with("list_")
        || action.starts_with("search_")
        || action.starts_with("read_")
        || action.starts_with("download_")
    {
        return None;
    }

    let is_guarded = action.contains("comment")
        || action.contains("review")
        || action.contains("issue")
        || action.contains("pull_request");
    if !is_guarded {
        return None;
    }

    let obj = tool_input.and_then(Value::as_object);

    if action.contains("review")
        && let Some(event) = obj.and_then(|m| m.get("event")).and_then(Value::as_str)
        && event.trim().eq_ignore_ascii_case("APPROVE")
    {
        return Some(deny(format!(
            "`{tool}` with `event: APPROVE` is forbidden for agents"
        )));
    }

    let mut found_body = false;
    if let Some(map) = obj {
        for key in ["body", "comment", "text"] {
            if let Some(val) = map.get(key) {
                if val.is_null() {
                    continue;
                }
                let Some(text) = val.as_str() else {
                    return Some(deny(format!(
                        "`{tool}` field `{key}` must be a string starting with `{BOT_MARKER}`"
                    )));
                };
                found_body = true;
                if !has_bot_marker(text) {
                    return Some(deny(format!(
                        "`{tool}` field `{key}` is missing the required `{BOT_MARKER}` prefix"
                    )));
                }
            }
        }
    }

    let body_required = action.contains("comment")
        || action.contains("review")
        || action.contains("create_issue")
        || action.contains("create_pull_request");
    if !found_body && body_required {
        return Some(deny(format!(
            "`{tool}` requires a body field starting with `{BOT_MARKER}`"
        )));
    }

    None
}
