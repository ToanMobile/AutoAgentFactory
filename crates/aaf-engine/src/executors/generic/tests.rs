use aaf_domain::account::ProviderKind;
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
        max_turns: None,
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
        max_turns: None,
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

#[test]
fn grok_account_closes_auth_config_hooks_and_binaries() {
    let paths = paths_for(ProviderKind::Grok, "/h/.grok");
    assert_eq!(
        paths.writable,
        vec![
            "/h/.grok/sessions".to_string(),
            "/h/.grok/logs".to_string(),
            "/h/.grok/memtrace".to_string(),
            "/h/.grok/process-records".to_string(),
            "/h/.grok/long-running-background-tasks".to_string(),
        ]
    );
    for name in [
        "auth.json",
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
        "settings_cache.json",
        "memory-v2",
        "completions",
        "downloads",
        "bin",
        "bundled",
        "bundles",
        "vendor",
        "worktrees",
        "worktrees.db",
        "leader.sock",
    ] {
        let entry = format!("/h/.grok/{name}");
        assert!(
            paths.write_denied.contains(&entry),
            "{entry} not closed: {:?}",
            paths.write_denied
        );
    }
}

#[test]
fn grok_sandbox_cli_flags_enforce_plan_and_bypass_permissions() {
    let runner = GenericRunner::new(
        vec![
            "grok".to_string(),
            "--no-memory".to_string(),
            "--single={prompt}".to_string(),
        ],
        ExecEnv::default(),
    )
    .unwrap();
    let mk_req = |enabled: bool, read_only: bool| AgentRequest {
        run_id: "r1".to_string(),
        cwd: "/tmp".to_string(),
        prompt: "fix".to_string(),
        account: None,
        timeout_secs: 60,
        sandbox: SandboxPolicy {
            enabled,
            read_only,
            ..SandboxPolicy::default()
        },
        json_schema: None,
        max_turns: None,
    };
    let ro = runner.command(&mk_req(true, true));
    assert_eq!(
        &ro.args[..3],
        &["--permission-mode", "plan", "--no-subagents"]
    );
    let sb = runner.command(&mk_req(true, false));
    assert_eq!(&sb.args[..2], &["--permission-mode", "bypassPermissions"]);
    let un = runner.command(&mk_req(false, false));
    assert_eq!(&un.args[..2], &["--permission-mode", "acceptEdits"]);

    // Read-only strips bypass and --no-plan flags even on custom executor_cmd templates.
    let custom = GenericRunner::new(
        vec![
            "grok".to_string(),
            "--always-approve".to_string(),
            "--no-plan".to_string(),
            "--permission-mode=bypassPermissions".to_string(),
            "--single={prompt}".to_string(),
        ],
        ExecEnv::default(),
    )
    .unwrap();
    let forced_ro = custom.command(&mk_req(true, true));
    assert_eq!(
        &forced_ro.args[..3],
        &["--permission-mode", "plan", "--no-subagents"]
    );
    assert!(
        !forced_ro.args.iter().any(|a| a == "--always-approve"
            || a == "--no-plan"
            || a.contains("bypassPermissions"))
    );
}
