#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;

use serde_json::json;

use super::{MAX_MCP_JSON_BYTES, McpTransport, builtin_roles, inspect_mcp_servers, load_roles};
use crate::executors::registry::find_descriptor;

fn write_file(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

#[test]
fn builtin_roles_include_four_starters_with_expected_read_only_flags() {
    let roles = builtin_roles();
    let names: Vec<&str> = roles.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["architect", "implementer", "reviewer", "qa-tester"]
    );

    let by_name = |name: &str| roles.iter().find(|r| r.name == name).unwrap();
    assert!(by_name("architect").read_only);
    assert!(!by_name("implementer").read_only);
    assert!(by_name("reviewer").read_only);
    assert!(!by_name("qa-tester").read_only);

    for role in &roles {
        assert!(
            find_descriptor(&role.executor).is_some(),
            "built-in role {} references unknown executor {}",
            role.name,
            role.executor
        );
        for fb in &role.fallback_executors {
            assert!(
                find_descriptor(fb).is_some(),
                "built-in role {} references unknown fallback {}",
                role.name,
                fb
            );
        }
    }
}

#[test]
fn load_roles_returns_builtins_when_dir_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let roles = load_roles(tmp.path()).unwrap();
    assert_eq!(roles, builtin_roles());
}

#[test]
fn load_roles_overlays_builtin_and_adds_custom_role() {
    let tmp = tempfile::tempdir().unwrap();
    write_file(
        tmp.path(),
        ".autofactory/roles/reviewer.toml",
        r#"
name = "reviewer"
description = "Custom repo reviewer"
executor = "claude-code"
fallback_executors = ["codex", "dsh"]
model = "claude-sonnet-4-6"
effort = "high"
read_only = true
skills = ["security"]
system_prompt = "Audit diffs strictly."
"#,
    );
    write_file(
        tmp.path(),
        ".autofactory/roles/sec-auditor.toml",
        r#"
name = "sec-auditor"
description = "Custom wrapper auditor"
executor = "custom"
read_only = true
"#,
    );

    let roles = load_roles(tmp.path()).unwrap();
    assert_eq!(roles.len(), 5);

    let reviewer = roles.iter().find(|r| r.name == "reviewer").unwrap();
    assert_eq!(reviewer.description, "Custom repo reviewer");
    assert_eq!(reviewer.executor, "claude");
    assert_eq!(reviewer.fallback_executors, vec!["codex", "deepseek"]);
    assert_eq!(reviewer.model.as_deref(), Some("claude-sonnet-4-6"));
    assert_eq!(reviewer.effort.as_deref(), Some("high"));
    assert!(reviewer.read_only);

    let custom = roles.iter().find(|r| r.name == "sec-auditor").unwrap();
    assert_eq!(custom.executor, "custom");
    assert!(custom.read_only);
}

#[test]
fn load_roles_rejects_unknown_executor_unknown_fields_and_flag_injection() {
    // Unknown executor
    let tmp1 = tempfile::tempdir().unwrap();
    write_file(
        tmp1.path(),
        ".autofactory/roles/bad.toml",
        "name = \"bad\"\nexecutor = \"nonexistent-cli\"\n",
    );
    let err = load_roles(tmp1.path()).unwrap_err();
    assert!(err.contains("unknown executor"), "{err}");

    // Unknown fallback executor
    let tmp2 = tempfile::tempdir().unwrap();
    write_file(
        tmp2.path(),
        ".autofactory/roles/bad_fb.toml",
        "name = \"bad-fb\"\nexecutor = \"claude\"\nfallback_executors = [\"bogus\"]\n",
    );
    let err = load_roles(tmp2.path()).unwrap_err();
    assert!(err.contains("unknown fallback_executors"), "{err}");

    // Unknown TOML field (deny_unknown_fields)
    let tmp3 = tempfile::tempdir().unwrap();
    write_file(
        tmp3.path(),
        ".autofactory/roles/extra.toml",
        "name = \"extra\"\nexecutor = \"claude\"\nsurprise_field = true\n",
    );
    let err = load_roles(tmp3.path()).unwrap_err();
    assert!(err.contains("unknown field"), "{err}");

    // Flag injection in model
    let tmp4 = tempfile::tempdir().unwrap();
    write_file(
        tmp4.path(),
        ".autofactory/roles/inject.toml",
        "name = \"inject\"\nexecutor = \"claude\"\nmodel = \"--model=-evil\"\n",
    );
    let err = load_roles(tmp4.path()).unwrap_err();
    assert!(err.contains("model"), "{err}");
}

#[cfg(unix)]
#[test]
fn load_roles_rejects_symlink_file_and_symlink_dir() {
    use std::os::unix::fs::symlink;

    // Symlinked TOML file inside `.autofactory/roles/`
    let tmp1 = tempfile::tempdir().unwrap();
    let target = tmp1.path().join("outside.toml");
    fs::write(&target, "name = \"linked\"\nexecutor = \"claude\"\n").unwrap();
    let roles_dir = tmp1.path().join(".autofactory/roles");
    fs::create_dir_all(&roles_dir).unwrap();
    symlink(&target, roles_dir.join("linked.toml")).unwrap();
    let err = load_roles(tmp1.path()).unwrap_err();
    assert!(err.contains("symlink"), "{err}");

    // Symlinked `.autofactory/roles` directory
    let tmp2 = tempfile::tempdir().unwrap();
    let real_dir = tmp2.path().join("real_roles");
    fs::create_dir_all(&real_dir).unwrap();
    fs::create_dir_all(tmp2.path().join(".autofactory")).unwrap();
    symlink(&real_dir, tmp2.path().join(".autofactory/roles")).unwrap();
    let err = load_roles(tmp2.path()).unwrap_err();
    assert!(err.contains("symlink"), "{err}");
}

#[test]
fn inspect_mcp_servers_absent_returns_clean_not_present() {
    let tmp = tempfile::tempdir().unwrap();
    let report = inspect_mcp_servers(tmp.path()).unwrap();
    assert!(!report.present);
    assert!(report.servers.is_empty());
    assert!(!report.has_warnings());
    assert_eq!(report.total_warnings(), 0);
}

#[test]
fn inspect_mcp_servers_clean_config_with_env_placeholders_passes() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = json!({
        "mcpServers": {
            "local-tools": {
                "command": "npx",
                "args": ["-y", "@example/mcp-server"],
                "env": {
                    "ANTHROPIC_API_KEY": "${ANTHROPIC_API_KEY}",
                    "GITHUB_TOKEN": "${env:GITHUB_TOKEN}",
                    "LOG_LEVEL": "info"
                }
            },
            "remote-api": {
                "type": "http",
                "url": "https://mcp.example.com/v1",
                "headers": {
                    "Authorization": "Bearer ${MCP_AUTH_TOKEN}",
                    "X-Api-Key": "${MCP_API_KEY}",
                    "Accept": "application/json"
                }
            }
        }
    });
    write_file(tmp.path(), ".mcp.json", &cfg.to_string());

    let report = inspect_mcp_servers(tmp.path()).unwrap();
    assert!(report.present);
    assert_eq!(report.servers.len(), 2);
    assert!(!report.has_warnings(), "{:?}", report.servers);

    let local = &report.servers[0];
    assert_eq!(local.name, "local-tools");
    assert_eq!(local.transport, McpTransport::Stdio);
    assert_eq!(local.command_or_url, "npx");
    assert_eq!(
        local.env_keys,
        vec!["ANTHROPIC_API_KEY", "GITHUB_TOKEN", "LOG_LEVEL"]
    );

    let remote = &report.servers[1];
    assert_eq!(remote.name, "remote-api");
    assert_eq!(remote.transport, McpTransport::Http);
    assert_eq!(remote.command_or_url, "https://mcp.example.com/v1");
}

#[test]
fn inspect_mcp_servers_flags_inline_secrets_without_echoing_raw_values() {
    let tmp = tempfile::tempdir().unwrap();
    // Assemble fake credentials from fragments so static scanners never flag test source.
    let ant_secret = ["sk-", "ant-api03-testsecret987654"].concat();
    let gh_secret = ["ghp_", "TestTokenValue1234567890"].concat();
    let plain_api_key = "plaintext-secret-value-42";
    let url_pass = "superSecretUrlPass99";
    let url_with_creds = format!("https://alice:{url_pass}@mcp.example.com/sse");

    let cfg = json!({
        "mcpServers": {
            "leaky-stdio": {
                "command": "node",
                "args": ["server.js"],
                "env": {
                    "CUSTOM_VAR": ant_secret,
                    "API_KEY": plain_api_key
                }
            },
            "leaky-sse": {
                "url": url_with_creds,
                "headers": {
                    "X-Custom": gh_secret
                }
            }
        }
    });
    write_file(tmp.path(), ".mcp.json", &cfg.to_string());

    let report = inspect_mcp_servers(tmp.path()).unwrap();
    assert!(report.present);
    assert!(report.has_warnings());
    assert_eq!(report.total_warnings(), 4);

    let stdio = report
        .servers
        .iter()
        .find(|s| s.name == "leaky-stdio")
        .unwrap();
    assert_eq!(stdio.inline_secret_warnings.len(), 2);

    let sse = report
        .servers
        .iter()
        .find(|s| s.name == "leaky-sse")
        .unwrap();
    assert_eq!(sse.transport, McpTransport::Sse);
    assert!(sse.command_or_url.contains("[REDACTED]"));
    assert!(!sse.command_or_url.contains(url_pass));
    assert_eq!(sse.inline_secret_warnings.len(), 2);

    let serialized = serde_json::to_string(&report).unwrap();
    for secret in [
        &ant_secret,
        &gh_secret,
        &plain_api_key.to_string(),
        &url_pass.to_string(),
    ] {
        assert!(
            !serialized.contains(secret.as_str()),
            "raw secret leaked into audit report: {serialized}"
        );
    }
}

#[test]
fn inspect_mcp_servers_rejects_malformed_json_and_oversized_files() {
    // Malformed JSON
    let tmp1 = tempfile::tempdir().unwrap();
    write_file(tmp1.path(), ".mcp.json", "{not valid json");
    let err = inspect_mcp_servers(tmp1.path()).unwrap_err();
    assert!(err.contains("malformed .mcp.json"), "{err}");

    // Wrong mcpServers type
    let tmp2 = tempfile::tempdir().unwrap();
    write_file(tmp2.path(), ".mcp.json", r#"{"mcpServers": []}"#);
    let err = inspect_mcp_servers(tmp2.path()).unwrap_err();
    assert!(err.contains("mcpServers"), "{err}");

    // Oversized (> 1 MiB)
    let tmp3 = tempfile::tempdir().unwrap();
    let big = " ".repeat(MAX_MCP_JSON_BYTES + 16);
    write_file(tmp3.path(), ".mcp.json", &big);
    let err = inspect_mcp_servers(tmp3.path()).unwrap_err();
    assert!(err.contains("exceeds maximum"), "{err}");
}

#[cfg(unix)]
#[test]
fn inspect_mcp_servers_rejects_symlink() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().unwrap();
    let outside = tmp.path().join("real.mcp.json");
    fs::write(&outside, r#"{"mcpServers": {}}"#).unwrap();
    symlink(&outside, tmp.path().join(".mcp.json")).unwrap();

    let err = inspect_mcp_servers(tmp.path()).unwrap_err();
    assert!(err.contains("symlink"), "{err}");
}

#[test]
fn rejects_oversized_system_prompt_and_invalid_skill_slugs() {
    use super::{MAX_SYSTEM_PROMPT_CHARS, validate_and_normalize_role};
    let mut role = builtin_roles().remove(0);
    role.system_prompt = "a".repeat(MAX_SYSTEM_PROMPT_CHARS + 1);
    let err = validate_and_normalize_role(role, Path::new("r.toml")).unwrap_err();
    assert!(err.contains("system_prompt"), "{err}");

    for bad in ["", "  ", "../escape", "--flag", "has space", "a/b"] {
        let mut r = builtin_roles().remove(0);
        r.skills = vec![bad.to_string()];
        let err = validate_and_normalize_role(r, Path::new("r.toml")).unwrap_err();
        assert!(err.contains("invalid skill slug"), "{bad}: {err}");
    }
    let mut ok_role = builtin_roles().remove(0);
    ok_role.skills = vec![" code-review ".to_string(), "qa_v2".to_string()];
    let norm = validate_and_normalize_role(ok_role, Path::new("r.toml")).unwrap();
    assert_eq!(norm.skills, vec!["code-review", "qa_v2"]);
}
