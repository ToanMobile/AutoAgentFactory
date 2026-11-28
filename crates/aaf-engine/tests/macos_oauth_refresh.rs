//! Regression tests for host-side macOS OAuth pre-flight/reactive refresh
//! helpers, nested minimal Seatbelt host-refresh profile, Claude `SESSION_STATE`
//! symlink confinement, and Antigravity sandbox rules.

use aaf_domain::ports::{InfraFailure, InfraKind};
use aaf_engine::executors::agy::parse_agy_output;
use aaf_engine::executors::macos_oauth_refresh::{
    claude_keychain_service, is_agy_auth_retryable, is_claude_auth_retryable,
    is_claude_keychain_save_error, non_json_lines, parse_claude_oauth_expiry_ms,
    reclassify_refreshed_keychain_failure,
};

#[test]
fn agy_empty_conversation_id_classifies_as_transient_network_infra() {
    let payload = r#"{"conversation_id":"","status":"ERROR","response":"","error":"failed to start conversation: backend returned an empty conversation id","duration_seconds":0}"#;
    let parsed = parse_agy_output(payload, true);
    assert!(parsed.is_error);
    assert!(parsed.limit_hit.is_none());
    assert_eq!(
        parsed.infra.as_ref().map(|i| i.kind),
        Some(InfraKind::Network)
    );
}

#[test]
fn agy_read_only_uses_plan_settings_and_never_skip_permissions() {
    use aaf_domain::ports::{AgentRequest, SandboxPolicy};
    use aaf_engine::executors::ExecEnv;
    use aaf_engine::executors::agy::{AGY_READ_ONLY_SETTINGS, AgyRunner};

    let runner = AgyRunner::new("/usr/local/bin/agy", ExecEnv::default());
    let make_req = |enabled: bool| AgentRequest {
        run_id: "test-ro".to_string(),
        prompt: "review".to_string(),
        cwd: "/tmp".to_string(),
        account: None,
        timeout_secs: 60,
        max_turns: None,
        sandbox: SandboxPolicy {
            enabled,
            read_only: true,
            ..SandboxPolicy::default()
        },
        json_schema: None,
    };
    for enabled in [true, false] {
        let spec = runner.command(&make_req(enabled)).expect("read_only plan");
        assert!(spec.args.windows(2).any(|w| w == ["--mode", "plan"]));
        assert!(
            spec.args
                .windows(2)
                .any(|w| w == ["--settings", AGY_READ_ONLY_SETTINGS])
        );
        assert!(
            !spec
                .args
                .iter()
                .any(|a| a == "--dangerously-skip-permissions"),
            "read_only must never pass --dangerously-skip-permissions (enabled={enabled})"
        );
    }
}

#[test]
fn claude_keychain_service_matches_claude_cli_sha256_prefix_convention() {
    assert_eq!(claude_keychain_service(None), "Claude Code-credentials");
    assert_eq!(
        claude_keychain_service(Some("   ")),
        "Claude Code-credentials"
    );
    assert_eq!(
        claude_keychain_service(Some("/Users/alex/.claude-finos")),
        "Claude Code-credentials-08fc04bc"
    );
    assert_eq!(
        claude_keychain_service(Some("/Users/alex/.claude-max")),
        "Claude Code-credentials-0f12add9"
    );
    assert_eq!(
        claude_keychain_service(Some("/Users/alex/.claude-finos_lua")),
        "Claude Code-credentials-5a4c5915"
    );
}

#[test]
fn parse_claude_oauth_expiry_extracts_timestamp_and_refresh_flag_fail_closed() {
    let valid =
        r#"{"claudeAiOauth":{"accessToken":"tok","refreshToken":"ref","expiresAt":1795800000000}}"#;
    assert_eq!(
        parse_claude_oauth_expiry_ms(valid),
        Some((1_795_800_000_000, true))
    );

    let no_refresh =
        r#"{"claudeAiOauth":{"accessToken":"tok","refreshToken":"","expiresAt":1795800000000}}"#;
    assert_eq!(
        parse_claude_oauth_expiry_ms(no_refresh),
        Some((1_795_800_000_000, false))
    );

    assert_eq!(parse_claude_oauth_expiry_ms("{}"), None);
    assert_eq!(parse_claude_oauth_expiry_ms("not-json"), None);
}

#[test]
fn claude_keychain_save_error_matches_single_line_and_rejects_multiline_or_truncated_json() {
    let real_err = "Failed to refresh OAuth token: can't save to the Keychain. If locked, run `security unlock-keychain`. If sandboxed, use `claude setup-token`.";
    assert!(is_claude_keychain_save_error(real_err));
    assert!(is_claude_keychain_save_error(
        "Could not refresh your login: can't save to the Keychain"
    ));
    assert!(is_claude_keychain_save_error(
        "auth: Failed to refresh OAuth token (Keychain denied)"
    ));
    // Unrelated lines far apart must NOT match across line boundaries.
    assert!(!is_claude_keychain_save_error(
        "line 1: Failed to refresh OAuth token\nline 200: macOS Keychain test"
    ));
    // JSON stream-json assistant prose filtered out by non_json_lines must not trigger.
    let json_prose = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"can't save to the Keychain"}]}}"#;
    assert!(!is_claude_keychain_save_error(&non_json_lines(json_prose)));

    // A byte-truncated 64 KiB tail starting mid-JSON must drop its leading fragment.
    let mut truncated = String::from("...truncated fragment can't save to the Keychain\"}\n");
    while truncated.len() < 64 * 1024 {
        truncated.push_str(r#"{"type":"assistant","message":{"content":[]}}"#);
        truncated.push('\n');
    }
    assert!(!is_claude_keychain_save_error(&non_json_lines(&truncated)));

    assert!(!is_claude_keychain_save_error("HTTP 401 invalid_api_key"));
    assert!(!is_claude_keychain_save_error(
        "Your organization has been disabled"
    ));

    // Structured InfraKind::Auth with expired OAuth access token is retryable,
    // while unrelated auth errors or non-Auth infra kinds are not.
    let expired_auth = InfraFailure {
        kind: InfraKind::Auth,
        raw: "API Error: 401 OAuth access token has expired. Re-authenticate to continue."
            .to_string(),
    };
    assert!(is_claude_auth_retryable(Some(&expired_auth), ""));
    let expired_short = InfraFailure {
        kind: InfraKind::Auth,
        raw: "OAuth token has expired".to_string(),
    };
    assert!(is_claude_auth_retryable(Some(&expired_short), ""));
    let bad_key = InfraFailure {
        kind: InfraKind::Auth,
        raw: "HTTP 401 invalid_api_key".to_string(),
    };
    assert!(!is_claude_auth_retryable(Some(&bad_key), ""));
    let net_err = InfraFailure {
        kind: InfraKind::Network,
        raw: "OAuth access token has expired".to_string(),
    };
    assert!(!is_claude_auth_retryable(
        Some(&net_err),
        "can't save to the Keychain"
    ));
}

#[test]
fn agy_auth_retryable_requires_keyring_or_antigravity_login_error_even_on_auth_infra() {
    // Generic Auth failures (e.g. from test output containing "not logged in" or 401)
    // must NOT trigger host refresh or Network reclassification.
    for non_keyring_auth in [
        "unauthenticated",
        "HTTP 401 Unauthorized",
        "test failed: user is not logged in",
        "invalid_api_key",
        "HTTP 403 permission_denied",
        "invalid_grant: token revoked",
    ] {
        let auth_infra = InfraFailure {
            kind: InfraKind::Auth,
            raw: non_keyring_auth.to_string(),
        };
        assert!(
            !is_agy_auth_retryable(Some(&auth_infra), ""),
            "non-keyring auth error must not trigger agy refresh: {non_keyring_auth}"
        );
    }

    // Explicit Antigravity login or keyring save errors MUST match on both paths.
    for keyring_err in [
        "error getting token source: You are not logged into Antigravity.",
        "Failed keyring SaveToken, falling back to file: exit status 161",
        "Failed to save refreshed token: open antigravity-oauth-token: operation not permitted",
        "Keyring SaveToken failed (exit status 161)",
    ] {
        let auth_infra = InfraFailure {
            kind: InfraKind::Auth,
            raw: keyring_err.to_string(),
        };
        assert!(is_agy_auth_retryable(Some(&auth_infra), ""));
        assert!(is_agy_auth_retryable(None, keyring_err));
    }

    // Network infra must never trigger agy auth retry even if tail has keyring text.
    let net_infra = InfraFailure {
        kind: InfraKind::Network,
        raw: "502 bad gateway".to_string(),
    };
    assert!(!is_agy_auth_retryable(
        Some(&net_infra),
        "Failed keyring SaveToken"
    ));
}

#[test]
fn mid_run_keychain_failure_reclassifies_to_typed_oauth_refreshed_kind() {
    let orig = Some(InfraFailure {
        kind: InfraKind::Auth,
        raw: "Failed to refresh OAuth token: can't save to the Keychain".to_string(),
    });
    let next = reclassify_refreshed_keychain_failure(orig).expect("some infra");
    assert_eq!(next.kind, InfraKind::OAuthRefreshed);
}

#[cfg(target_os = "macos")]
#[test]
fn agy_seatbelt_allows_atomic_jetbox_tmp_but_denies_oauth_token_file() {
    use std::fs;
    use std::process::Command;

    use aaf_domain::ports::{AgentRequest, SandboxPolicy};
    use aaf_engine::executors::ExecEnv;
    use aaf_engine::executors::agy::AgyRunner;
    use aaf_engine::executors::sandbox::{SandboxPaths, seatbelt_profile};

    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path();
    let cli_dir = home.join(".gemini/antigravity-cli");
    fs::create_dir_all(&cli_dir).expect("mkdir");
    fs::write(cli_dir.join("antigravity-oauth-token"), "secret").expect("seed");

    let env = ExecEnv {
        home: Some(home.to_string_lossy().into_owned()),
        ..ExecEnv::default()
    };
    let runner = AgyRunner::new("/bin/agy", env);
    let policy = SandboxPolicy {
        enabled: true,
        read_only: true,
        ..SandboxPolicy::default()
    };
    let req = AgentRequest {
        run_id: String::new(),
        prompt: "hi".to_string(),
        cwd: home.to_string_lossy().into_owned(),
        account: None,
        timeout_secs: 60,
        max_turns: None,
        sandbox: policy.clone(),
        json_schema: None,
    };
    let acct = runner.account_paths(&req).expect("paths");
    let profile = seatbelt_profile(
        &policy,
        &SandboxPaths {
            writable: acct.writable,
            writable_regex: acct.writable_regex,
            write_denied: acct.write_denied,
            ..SandboxPaths::default()
        },
    )
    .expect("profile");

    let try_write = |target: &std::path::Path| -> bool {
        Command::new("/usr/bin/sandbox-exec")
            .args([
                "-p",
                &profile,
                "--",
                "/bin/sh",
                "-c",
                "printf ok > \"$1\"",
                "_",
                &target.to_string_lossy(),
            ])
            .status()
            .expect("sandbox-exec")
            .success()
    };

    assert!(
        try_write(
            &cli_dir.join("jetbox_summaries_proto.pb.1a8eb4e5-6a08-435f-a3eb-3ea4a85652cb.tmp")
        ),
        "atomic jetbox summary temp file should be writable inside Seatbelt"
    );
    assert!(
        !try_write(&cli_dir.join("antigravity-oauth-token")),
        "antigravity-oauth-token must remain strictly write-denied inside Seatbelt"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn host_refresh_minimal_seatbelt_reopens_nested_empty_cwd_inside_denied_data_dir_and_fails_closed()
{
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use aaf_engine::executors::macos_oauth_refresh::host_refresh_seatbelt_profile;

    let tmp = tempfile::tempdir().expect("tempdir");
    let data_dir = tmp.path().join("data");
    // Exact production layout: empty_cwd is nested inside data_dir/oauth-refresh-tmp/
    let empty_cwd = data_dir.join("oauth-refresh-tmp/r-abcdef123456");
    let kc_dir = tmp.path().join("keychains");
    let claude_cfg = tmp.path().join("claude-acct");
    let ssh_dir = tmp.path().join("ssh");
    let task_cwd = tmp.path().join("task");
    let other_dir = tmp.path().join("other");
    for d in [
        &empty_cwd,
        &kc_dir,
        &claude_cfg,
        &data_dir,
        &ssh_dir,
        &task_cwd,
        &other_dir,
    ] {
        fs::create_dir_all(d).expect("mkdir");
    }
    fs::write(data_dir.join("token"), "secret").expect("seed data");
    fs::write(ssh_dir.join("id_ed25519"), "secret").expect("seed ssh");
    fs::write(task_cwd.join("repo.rs"), "fn main() {}").expect("seed task");
    fs::write(claude_cfg.join("settings.json"), "{}").expect("seed claude settings");

    // Fail-closed: unquotable deny path or claude_config_dir overlapping denied_dirs must abort.
    let bad_path = Path::new("/tmp/bad\"quote");
    assert!(
        host_refresh_seatbelt_profile(&empty_cwd, &kc_dir, Some(&claude_cfg), &[bad_path])
            .is_none(),
        "unquotable deny path must fail closed"
    );
    assert!(
        host_refresh_seatbelt_profile(&empty_cwd, &kc_dir, Some(&ssh_dir), &[&ssh_dir]).is_none(),
        "claude_config_dir overlapping a denied dir must fail closed"
    );

    let profile = host_refresh_seatbelt_profile(
        &empty_cwd,
        &kc_dir,
        Some(&claude_cfg),
        &[&data_dir, &ssh_dir, &task_cwd],
    )
    .expect("profile");

    let run_sh = |script: &str, arg: &Path| -> bool {
        Command::new("/usr/bin/sandbox-exec")
            .args([
                "-p",
                &profile,
                "--",
                "/bin/sh",
                "-c",
                script,
                "_",
                &arg.to_string_lossy(),
            ])
            .status()
            .expect("sandbox-exec")
            .success()
    };

    assert!(
        run_sh(
            "printf ok > \"$1\" && cat \"$1\" >/dev/null",
            &empty_cwd.join("ok")
        ),
        "nested empty_cwd inside data_dir must be both readable and writable"
    );
    assert!(run_sh("printf ok > \"$1\"", &kc_dir.join("ok")));
    // Claude OAuth refresh locks MUST be writable inside minimal Seatbelt,
    // while settings.json and sibling files inside claude_config_dir stay strictly read-only.
    assert!(run_sh(
        "mkdir \"$1\" && rmdir \"$1\"",
        &claude_cfg.join(".oauth_refresh.lock")
    ));
    assert!(run_sh(
        "mkdir \"$1\" && rmdir \"$1\"",
        &claude_cfg.join(".storage-write.lock")
    ));
    assert!(run_sh(
        "mkdir \"$1\" && rmdir \"$1\"",
        &tmp.path().join("claude-acct.lock")
    ));
    assert!(run_sh(
        "printf ok > \"$1\" && rm \"$1\"",
        &claude_cfg.join(".oauth_refresh.lock.owner.tmp")
    ));
    assert!(
        !run_sh("printf bad > \"$1\"", &claude_cfg.join("settings.json")),
        "claude settings.json must remain strictly write-denied"
    );
    assert!(!run_sh("printf bad > \"$1\"", &other_dir.join("bad")));
    assert!(!run_sh("printf bad > \"$1\"", &task_cwd.join("bad")));
    assert!(!run_sh(
        "cat \"$1\" >/dev/null 2>&1",
        &data_dir.join("token")
    ));
    assert!(!run_sh(
        "cat \"$1\" >/dev/null 2>&1",
        &ssh_dir.join("id_ed25519")
    ));
    assert!(!run_sh(
        "cat \"$1\" >/dev/null 2>&1",
        &task_cwd.join("repo.rs")
    ));
}

#[cfg(target_os = "macos")]
#[test]
fn claude_session_state_symlink_cannot_open_keychains_or_ssh_for_write() {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    use aaf_domain::account::{AccountProfile, ProviderKind};
    use aaf_domain::ports::{AgentRequest, AgentRunner, SandboxPolicy};
    use aaf_engine::executors::ExecEnv;
    use aaf_engine::executors::claude::ClaudeCodeRunner;

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canon");
    let home = root.join("home");
    let kc = home.join("Library/Keychains");
    let ssh = home.join(".ssh");
    let desktop_todos = home.join("Desktop/todos");
    let shared_claude = home.join(".claude");
    let acct = root.join("acct");
    let wt = root.join("wt");
    for d in [&kc, &ssh, &desktop_todos, &shared_claude, &acct, &wt] {
        fs::create_dir_all(d).expect("mkdir");
    }
    fs::write(home.join("Desktop/secret.txt"), "top-secret").expect("seed desktop");
    // Attacker plants direct symlinks inside CLAUDE_CONFIG_DIR pointing to Keychains, .ssh,
    // and Desktop/todos, PLUS a chained SESSION_STATE symlink acct/session-env -> home/.claude/session-env -> Keychains.
    symlink(&kc, acct.join("projects")).expect("link kc");
    symlink(&ssh, acct.join("sessions")).expect("link ssh");
    symlink(&desktop_todos, acct.join("todos")).expect("link desktop todos");
    symlink(&kc, shared_claude.join("session-env")).expect("chain shared->kc");
    symlink(shared_claude.join("session-env"), acct.join("session-env"))
        .expect("chain acct->shared");

    let claude = root.join("claude");
    fs::write(
        &claude,
        format!(
            "#!/bin/sh\n\
             (echo pwn > \"$CLAUDE_CONFIG_DIR/projects/pwn\" 2>/dev/null && echo kc-pwn) || echo kc-denied\n\
             (echo pwn > \"$CLAUDE_CONFIG_DIR/sessions/pwn\" 2>/dev/null && echo ssh-pwn) || echo ssh-denied\n\
             (echo pwn > \"$CLAUDE_CONFIG_DIR/todos/pwn\" 2>/dev/null && echo dt-w-pwn) || echo dt-w-denied\n\
             (echo pwn > \"$CLAUDE_CONFIG_DIR/session-env/pwn\" 2>/dev/null && echo chain-pwn) || echo chain-denied\n\
             (cat \"{}/Desktop/secret.txt\" >/dev/null 2>&1 && echo dt-r-pwn) || echo dt-r-denied\n",
            home.display()
        ),
    )
    .expect("write script");
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).expect("chmod");

    let env = ExecEnv {
        home: Some(home.to_string_lossy().into_owned()),
        temp_dirs: Vec::new(),
        ..ExecEnv::default()
    };
    let out = ClaudeCodeRunner::new(claude.to_string_lossy(), env)
        .run(&AgentRequest {
            run_id: "test-symlink-escape".to_string(),
            prompt: "hi".to_string(),
            cwd: wt.to_string_lossy().into_owned(),
            account: Some(AccountProfile {
                id: "claude_code-evil".to_string(),
                provider: ProviderKind::ClaudeCode,
                config_dir: acct.to_string_lossy().into_owned(),
                name: "evil".to_string(),
            }),
            timeout_secs: 30,
            max_turns: None,
            sandbox: SandboxPolicy {
                enabled: true,
                read_only: true,
                ..SandboxPolicy::default()
            },
            json_schema: None,
        })
        .expect("run");

    for marker in [
        "kc-denied",
        "ssh-denied",
        "dt-w-denied",
        "chain-denied",
        "dt-r-denied",
    ] {
        assert!(
            out.log_tail.contains(marker),
            "missing {marker} in tail: {}",
            out.log_tail
        );
    }
    assert!(!kc.join("pwn").exists());
    assert!(!ssh.join("pwn").exists());
    assert!(!desktop_todos.join("pwn").exists());
}
