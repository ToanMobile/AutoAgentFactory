#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;

use aaf_engine::hook_settings::multi_host::{
    CODEX_HOOKS_REL, GEMINI_SETTINGS_REL, HookHostTarget, HostHookStatus, OPENCODE_HOOKS_REL,
    check_antigravity, check_codex, check_gemini, check_host_if_present, check_opencode,
    codex_matcher, covering_codex_hook, covering_gemini_hook, gemini_matcher, parse_host_arg,
    register_antigravity, register_codex, register_gemini, register_opencode,
};
use aaf_engine::hook_settings::{HookChange, SETTINGS_REL, merge_hook, validate_for_executor};
use serde_json::{Value, json};

fn codex_json(matcher: &str, command: &str) -> String {
    json!({
        "hooks": {
            "PreToolUse": [{
                "matcher": matcher,
                "hooks": [{"type": "command", "command": command}],
            }]
        }
    })
    .to_string()
}

fn gemini_json(matcher: &str, command: &str) -> String {
    json!({
        "hooks": {
            "BeforeTool": [{
                "matcher": matcher,
                "hooks": [{"type": "command", "command": command}],
            }]
        }
    })
    .to_string()
}

#[test]
fn parse_host_arg_valid_and_invalid() {
    assert_eq!(parse_host_arg("claude"), Ok(vec![HookHostTarget::Claude]));
    assert_eq!(parse_host_arg("CODEX"), Ok(vec![HookHostTarget::Codex]));
    assert_eq!(parse_host_arg(" gemini "), Ok(vec![HookHostTarget::Gemini]));
    assert_eq!(
        parse_host_arg("antigravity"),
        Ok(vec![HookHostTarget::Antigravity])
    );
    assert_eq!(
        parse_host_arg("opencode"),
        Ok(vec![HookHostTarget::Opencode])
    );
    assert_eq!(
        parse_host_arg("all"),
        Ok(vec![
            HookHostTarget::Claude,
            HookHostTarget::Codex,
            HookHostTarget::Gemini,
            HookHostTarget::Opencode,
        ])
    );
    assert_eq!(
        parse_host_arg("codex,gemini,codex"),
        Ok(vec![HookHostTarget::Codex, HookHostTarget::Gemini])
    );

    assert_eq!(HookHostTarget::Claude.as_str(), "claude");
    assert_eq!(HookHostTarget::Codex.as_str(), "codex");
    assert_eq!(HookHostTarget::Gemini.as_str(), "gemini");
    assert_eq!(HookHostTarget::Antigravity.as_str(), "antigravity");
    assert_eq!(HookHostTarget::Opencode.as_str(), "opencode");
    assert_eq!(HookHostTarget::Claude.settings_rel(), SETTINGS_REL);
    assert_eq!(HookHostTarget::Codex.settings_rel(), CODEX_HOOKS_REL);
    assert_eq!(HookHostTarget::Gemini.settings_rel(), GEMINI_SETTINGS_REL);
    assert_eq!(
        HookHostTarget::Antigravity.settings_rel(),
        GEMINI_SETTINGS_REL
    );
    assert_eq!(HookHostTarget::Opencode.settings_rel(), OPENCODE_HOOKS_REL);

    for bad in ["", "   ", "cursor", "claude,unknown"] {
        assert!(parse_host_arg(bad).is_err(), "expected Err for {bad:?}");
    }
}

#[test]
fn register_codex_is_idempotent_and_preserves_user_hooks() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Seed an existing `.codex/hooks.json` with an unrelated user hook and top-level key.
    let path = root.join(CODEX_HOOKS_REL);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        json!({
            "version": 1,
            "hooks": {
                "PreToolUse": [{
                    "matcher": "shell",
                    "hooks": [{"type": "command", "command": "my-custom-lint"}]
                }],
                "PostToolUse": [{
                    "matcher": "*",
                    "hooks": [{"type": "command", "command": "my-notifier"}]
                }]
            }
        })
        .to_string(),
    )
    .unwrap();

    // Added on first registration.
    assert_eq!(register_codex(root, "aaf-hook").unwrap(), HookChange::Added);
    let reg = check_codex(root).unwrap();
    assert_eq!(reg.matcher, codex_matcher());
    assert_eq!(reg.command, "aaf-hook --host codex");

    // Verify unrelated keys/hooks survived and `timeout` was omitted.
    let parsed: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(parsed["version"], 1);
    assert_eq!(
        parsed["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
        "my-notifier"
    );
    let pre = parsed["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre.len(), 2);
    assert_eq!(pre[0]["hooks"][0]["command"], "my-custom-lint");
    assert!(pre[1]["hooks"][0].get("timeout").is_none());

    // Unchanged on repeat registration with same command.
    assert_eq!(
        register_codex(root, "aaf-hook").unwrap(),
        HookChange::Unchanged
    );

    // Updated when binary path changes (including spaces needing shell quoting).
    assert_eq!(
        register_codex(root, "/opt/my tools/aaf-hook").unwrap(),
        HookChange::Updated
    );
    let reg = check_codex(root).unwrap();
    assert_eq!(reg.command, "'/opt/my tools/aaf-hook' --host codex");

    // Repeat with same quoted path is Unchanged again.
    assert_eq!(
        register_codex(root, "/opt/my tools/aaf-hook").unwrap(),
        HookChange::Unchanged
    );

    // Tampered entry is healed back to a valid registration without dropping user hooks.
    fs::write(
        &path,
        codex_json(&codex_matcher(), "aaf-hook --host codex || true"),
    )
    .unwrap();
    assert!(check_codex(root).is_err());
    assert_eq!(
        register_codex(root, "aaf-hook").unwrap(),
        HookChange::Updated
    );
    assert!(check_codex(root).is_ok());
}

#[test]
fn register_gemini_is_idempotent_and_preserves_user_settings() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let path = root.join(GEMINI_SETTINGS_REL);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        json!({
            "theme": "Default",
            "hooks": {
                "BeforeTool": [{
                    "matcher": "read_file",
                    "hooks": [{"type": "command", "command": "user-audit"}]
                }],
                "AfterTool": [{
                    "matcher": ".*",
                    "hooks": [{"type": "command", "command": "user-post"}]
                }]
            }
        })
        .to_string(),
    )
    .unwrap();

    assert_eq!(
        register_gemini(root, "aaf-hook").unwrap(),
        HookChange::Added
    );
    let reg = check_gemini(root).unwrap();
    assert_eq!(reg.matcher, gemini_matcher());
    assert_eq!(reg.command, "aaf-hook --host gemini");

    let parsed: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(parsed["theme"], "Default");
    assert_eq!(
        parsed["hooks"]["AfterTool"][0]["hooks"][0]["command"],
        "user-post"
    );
    let before = parsed["hooks"]["BeforeTool"].as_array().unwrap();
    assert_eq!(before.len(), 2);
    assert_eq!(before[0]["hooks"][0]["command"], "user-audit");
    assert!(before[1]["hooks"][0].get("timeout").is_none());

    assert_eq!(
        register_gemini(root, "aaf-hook").unwrap(),
        HookChange::Unchanged
    );

    assert_eq!(
        register_gemini(root, "/usr/local/bin/aaf-hook").unwrap(),
        HookChange::Updated
    );
    let reg = check_gemini(root).unwrap();
    assert_eq!(reg.command, "/usr/local/bin/aaf-hook --host gemini");
}

#[test]
fn check_codex_allows_valid_variants_and_rejects_tampering() {
    let dir = tempfile::tempdir().unwrap();
    assert!(check_codex(dir.path()).is_err());

    // Valid commands and matchers.
    for (matcher, cmd) in [
        (codex_matcher(), "aaf-hook --host codex"),
        (codex_matcher(), "/usr/local/bin/aaf-hook --host=codex"),
        ("*".to_string(), "'/opt/my tools/aaf-hook' --host codex"),
        (".*".to_string(), "\"/opt/my tools/aaf-hook\" --host=codex"),
        ("".to_string(), "  aaf-hook   --host   codex  "),
    ] {
        let text = codex_json(&matcher, cmd);
        assert!(covering_codex_hook(&text).is_ok(), "{cmd:?}");
    }

    // Valid timeout >= 30s is accepted; < 30s is rejected.
    let mut with_timeout: Value =
        serde_json::from_str(&codex_json(&codex_matcher(), "aaf-hook --host codex")).unwrap();
    with_timeout["hooks"]["PreToolUse"][0]["hooks"][0]["timeout"] = json!(60);
    assert!(covering_codex_hook(&with_timeout.to_string()).is_ok());

    with_timeout["hooks"]["PreToolUse"][0]["hooks"][0]["timeout"] = json!(10);
    assert!(covering_codex_hook(&with_timeout.to_string()).is_err());

    // Reject narrow matcher.
    let narrow = codex_json("Bash|shell", "aaf-hook --host codex");
    assert!(covering_codex_hook(&narrow).is_err());

    // Reject wrong host flag, missing host flag, shell chaining, and extra flags.
    for bad_cmd in [
        "aaf-hook",
        "aaf-hook --host gemini",
        "aaf-hook --host claude",
        "aaf-hook --host auto",
        "aaf-hook --host codex --dry-run",
        "aaf-hook --host codex || true",
        "aaf-hook --host codex && true",
        "aaf-hook --host codex; true",
        "aaf-hook --host codex | cat",
        "aaf-hook --host codex >/dev/null",
        "aaf-hook --host codex\ntrue",
        "$(echo /x)/aaf-hook --host codex",
        "`echo /x`/aaf-hook --host codex",
        "FOO=1 aaf-hook --host codex",
    ] {
        let text = codex_json(&codex_matcher(), bad_cmd);
        assert!(
            covering_codex_hook(&text).is_err(),
            "expected Codex reject for {bad_cmd:?}"
        );
    }

    // Reject bad JSON, non-object JSON, `disableAllHooks`, non-blocking entry keys, and forbidden env.
    assert!(covering_codex_hook("not json").is_err());
    assert!(covering_codex_hook("[]").is_err());

    let mut disabled: Value =
        serde_json::from_str(&codex_json(&codex_matcher(), "aaf-hook --host codex")).unwrap();
    disabled["disableAllHooks"] = json!(true);
    assert!(covering_codex_hook(&disabled.to_string()).is_err());

    let mut async_hook: Value =
        serde_json::from_str(&codex_json(&codex_matcher(), "aaf-hook --host codex")).unwrap();
    async_hook["hooks"]["PreToolUse"][0]["hooks"][0]["async"] = json!(true);
    assert!(covering_codex_hook(&async_hook.to_string()).is_err());

    let mut bad_env: Value =
        serde_json::from_str(&codex_json(&codex_matcher(), "aaf-hook --host codex")).unwrap();
    bad_env["env"] = json!({"AAF_DATA_DIR": "/tmp/fake"});
    assert!(covering_codex_hook(&bad_env.to_string()).is_err());

    let mut path_env: Value =
        serde_json::from_str(&codex_json(&codex_matcher(), "aaf-hook --host codex")).unwrap();
    path_env["env"] = json!({"PATH": "/tmp/evil"});
    assert!(covering_codex_hook(&path_env.to_string()).is_err());
}

#[test]
fn check_gemini_allows_valid_variants_and_rejects_tampering() {
    let dir = tempfile::tempdir().unwrap();
    assert!(check_gemini(dir.path()).is_err());

    for (matcher, cmd) in [
        (gemini_matcher(), "aaf-hook --host gemini"),
        (gemini_matcher(), "/usr/local/bin/aaf-hook --host=gemini"),
        (".*".to_string(), "'/opt/my tools/aaf-hook' --host gemini"),
        ("*".to_string(), "aaf-hook --host gemini"),
    ] {
        let text = gemini_json(&matcher, cmd);
        assert!(covering_gemini_hook(&text).is_ok(), "{cmd:?}");
    }

    // Gemini timeout is in milliseconds: >= 30_000 ok, < 30_000 rejected.
    let mut with_timeout: Value =
        serde_json::from_str(&gemini_json(&gemini_matcher(), "aaf-hook --host gemini")).unwrap();
    with_timeout["hooks"]["BeforeTool"][0]["hooks"][0]["timeout"] = json!(60_000);
    assert!(covering_gemini_hook(&with_timeout.to_string()).is_ok());

    with_timeout["hooks"]["BeforeTool"][0]["hooks"][0]["timeout"] = json!(5_000);
    assert!(covering_gemini_hook(&with_timeout.to_string()).is_err());

    // Reject narrow matcher.
    let narrow = gemini_json("run_shell_command|read_file", "aaf-hook --host gemini");
    assert!(covering_gemini_hook(&narrow).is_err());

    // Reject wrong host flag and shell injection.
    for bad_cmd in [
        "aaf-hook",
        "aaf-hook --host codex",
        "aaf-hook --host gemini || true",
        "aaf-hook --host gemini; id",
        "aaf-hook --host gemini && true",
        "aaf-hook --host gemini | cat",
        "$(id)/aaf-hook --host gemini",
        "`id`/aaf-hook --host gemini",
    ] {
        let text = gemini_json(&gemini_matcher(), bad_cmd);
        assert!(
            covering_gemini_hook(&text).is_err(),
            "expected Gemini reject for {bad_cmd:?}"
        );
    }

    // Reject `hooksConfig.enabled: false` and `disableAllHooks: true`.
    let mut disabled_cfg: Value =
        serde_json::from_str(&gemini_json(&gemini_matcher(), "aaf-hook --host gemini")).unwrap();
    disabled_cfg["hooksConfig"] = json!({"enabled": false});
    assert!(covering_gemini_hook(&disabled_cfg.to_string()).is_err());

    disabled_cfg["hooksConfig"] = json!({"enabled": true});
    assert!(covering_gemini_hook(&disabled_cfg.to_string()).is_ok());
}

#[test]
fn check_host_if_present_reports_not_configured_registered_and_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Fresh repo: all 3 hosts are NotConfigured.
    for host in [
        HookHostTarget::Claude,
        HookHostTarget::Codex,
        HookHostTarget::Gemini,
    ] {
        assert_eq!(
            check_host_if_present(root, host),
            HostHookStatus::NotConfigured
        );
    }

    // Register all 3 hosts -> all 3 report Registered.
    let (claude_text, _) = merge_hook(None, "aaf-hook").unwrap();
    let claude_path = root.join(SETTINGS_REL);
    fs::create_dir_all(claude_path.parent().unwrap()).unwrap();
    fs::write(&claude_path, claude_text.unwrap()).unwrap();
    register_codex(root, "aaf-hook").unwrap();
    register_gemini(root, "aaf-hook").unwrap();

    for host in [
        HookHostTarget::Claude,
        HookHostTarget::Codex,
        HookHostTarget::Gemini,
    ] {
        assert!(
            matches!(
                check_host_if_present(root, host),
                HostHookStatus::Registered(_)
            ),
            "expected Registered for {host:?}"
        );
    }

    // Tamper each host file -> all 3 report Invalid.
    fs::write(root.join(SETTINGS_REL), "{not valid json").unwrap();
    fs::write(
        root.join(CODEX_HOOKS_REL),
        codex_json("Bash", "aaf-hook --host codex"),
    )
    .unwrap();
    fs::write(
        root.join(GEMINI_SETTINGS_REL),
        gemini_json(&gemini_matcher(), "aaf-hook --host codex"),
    )
    .unwrap();

    for host in [
        HookHostTarget::Claude,
        HookHostTarget::Codex,
        HookHostTarget::Gemini,
    ] {
        assert!(
            matches!(
                check_host_if_present(root, host),
                HostHookStatus::Invalid(_)
            ),
            "expected Invalid for {host:?}"
        );
    }
}

#[test]
fn antigravity_opencode_and_validate_for_executor() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    assert!(validate_for_executor(root, "custom").is_ok());
    assert!(validate_for_executor(root, "claude").is_err());
    assert!(validate_for_executor(root, "codex").is_err());
    assert!(validate_for_executor(root, "gemini").is_err());
    assert!(validate_for_executor(root, "antigravity").is_err());
    assert!(validate_for_executor(root, "opencode").is_err());

    let (claude_text, _) = merge_hook(None, "aaf-hook").unwrap();
    let claude_path = root.join(SETTINGS_REL);
    fs::create_dir_all(claude_path.parent().unwrap()).unwrap();
    fs::write(&claude_path, claude_text.unwrap()).unwrap();
    assert!(validate_for_executor(root, "claude").is_ok());

    assert_eq!(register_codex(root, "aaf-hook").unwrap(), HookChange::Added);
    assert!(validate_for_executor(root, "codex").is_ok());

    assert_eq!(
        register_antigravity(root, "aaf-hook").unwrap(),
        HookChange::Added
    );
    assert!(check_antigravity(root).is_ok());
    assert!(validate_for_executor(root, "antigravity").is_ok());
    assert!(validate_for_executor(root, "gemini").is_ok());

    assert_eq!(
        register_opencode(root, "aaf-hook").unwrap(),
        HookChange::Added
    );
    assert!(check_opencode(root).is_ok());
    assert!(validate_for_executor(root, "opencode").is_ok());
    assert_eq!(
        register_opencode(root, "aaf-hook").unwrap(),
        HookChange::Unchanged
    );
}
