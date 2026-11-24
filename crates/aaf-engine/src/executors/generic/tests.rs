use aaf_domain::ports::SandboxPolicy;

use super::*;

fn paths_for(provider: ProviderKind, dir: &str) -> AccountPaths {
    account_paths(&AgentRequest {
        run_id: "r1".to_string(),
        cwd: "/tmp".to_string(),
        prompt: "audit".to_string(),
        account: Some(aaf_domain::account::AccountProfile {
            id: "acct".to_string(),
            provider,
            config_dir: dir.to_string(),
            name: "acct".to_string(),
        }),
        timeout_secs: 60,
        sandbox: SandboxPolicy::default(),
        json_schema: None,
    })
}

/// R10-L7-1 + Plan v3 W1: Gemini / Antigravity account narrows writable paths
/// to `tmp` and `history` and closes hooks, settings, credentials, and executables.
#[test]
fn antigravity_account_closes_hooks_settings_and_executables() {
    let paths = paths_for(ProviderKind::Antigravity, "/h/.gemini");
    assert_eq!(
        paths.writable,
        vec![
            "/h/.gemini/tmp".to_string(),
            "/h/.gemini/history".to_string()
        ]
    );
    for name in [
        "settings.json",
        "settings.local.json",
        "trusted_hooks.json",
        "trustedFolders.json",
        "GEMINI.md",
        "oauth_creds.json",
        "hooks",
        "extensions",
        "policies",
        "commands",
        "antigravity/bin",
        "antigravity/mcp",
        "antigravity-cli/bin",
        "antigravity-cli/mcp",
        "antigravity-cli/settings.json",
        "config/hooks.json",
        "config/mcp_config.json",
    ] {
        let entry = format!("/h/.gemini/{name}");
        assert!(
            paths.write_denied.contains(&entry),
            "{entry} not closed: {:?}",
            paths.write_denied
        );
    }
    for name in ["tmp", "history"] {
        let entry = format!("/h/.gemini/{name}");
        assert!(!paths.write_denied.contains(&entry), "{entry} closed");
    }
}

#[test]
fn opencode_account_closes_config_and_plugins() {
    let paths = paths_for(ProviderKind::OpenCode, "/h/.config/opencode");
    for name in [
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
    ] {
        let entry = format!("/h/.config/opencode/{name}");
        assert!(
            paths.write_denied.contains(&entry),
            "{entry} not closed: {:?}",
            paths.write_denied
        );
    }
}

#[test]
fn gemini_command_sets_headless_workspace_trust_env() {
    let req = AgentRequest {
        run_id: "r1".to_string(),
        cwd: "/tmp".to_string(),
        prompt: "audit".to_string(),
        account: None,
        timeout_secs: 60,
        sandbox: SandboxPolicy::default(),
        json_schema: None,
    };
    let runner = GenericRunner::new(
        vec![
            "gemini".to_string(),
            "-p".to_string(),
            "{prompt}".to_string(),
        ],
        ExecEnv::default(),
    )
    .unwrap();
    let spec = runner.command(&req);
    assert!(
        spec.env
            .contains(&("GEMINI_CLI_TRUST_WORKSPACE".to_string(), "true".to_string())),
        "{:?}",
        spec.env
    );
}
