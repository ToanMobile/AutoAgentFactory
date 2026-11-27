//! Regression tests for host-side macOS OAuth pre-flight/reactive refresh
//! helpers, minimal Seatbelt host-refresh profile, and Antigravity sandbox rules.

use aaf_domain::ports::{InfraFailure, InfraKind};
use aaf_engine::executors::macos_oauth_refresh::{
    OAUTH_REFRESHED_MARKER, claude_keychain_service, is_agy_auth_retryable,
    is_claude_keychain_save_error, non_json_lines, parse_claude_oauth_expiry_ms,
    reclassify_refreshed_keychain_failure,
};

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
fn claude_keychain_save_error_matches_single_line_and_rejects_multiline_or_json_prose() {
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
    assert!(!is_claude_keychain_save_error("HTTP 401 invalid_api_key"));
    assert!(!is_claude_keychain_save_error(
        "Your organization has been disabled"
    ));
}

#[test]
fn agy_auth_retryable_matches_keyring_and_antigravity_login_and_excludes_permanent_auth() {
    let auth_infra = InfraFailure {
        kind: InfraKind::Auth,
        raw: "unauthenticated".to_string(),
    };
    assert!(is_agy_auth_retryable(Some(&auth_infra), ""));
    assert!(is_agy_auth_retryable(
        None,
        "error getting token source: You are not logged into Antigravity."
    ));
    assert!(is_agy_auth_retryable(
        None,
        "Failed keyring SaveToken, falling back to file: exit status 161"
    ));
    assert!(is_agy_auth_retryable(
        None,
        "Failed to save refreshed token: open antigravity-oauth-token: operation not permitted"
    ));
    // Generic bare "not logged in" prose must NOT trigger host refresh.
    assert!(!is_agy_auth_retryable(
        None,
        "test failed: user is not logged in on staging"
    ));
    // Permanent non-refreshable auth errors must NOT trigger host refresh.
    for perm in [
        "invalid_api_key",
        "HTTP 403 permission_denied",
        "invalid_grant: token revoked",
    ] {
        let perm_infra = InfraFailure {
            kind: InfraKind::Auth,
            raw: perm.to_string(),
        };
        assert!(
            !is_agy_auth_retryable(Some(&perm_infra), ""),
            "permanent auth error must not retry: {perm}"
        );
    }
    let net_infra = InfraFailure {
        kind: InfraKind::Network,
        raw: "502 bad gateway".to_string(),
    };
    assert!(!is_agy_auth_retryable(
        Some(&net_infra),
        "build failed: exit status 1"
    ));
}

#[test]
fn mid_run_keychain_failure_reclassifies_to_transient_network_with_stable_marker() {
    let orig = Some(InfraFailure {
        kind: InfraKind::Auth,
        raw: "Failed to refresh OAuth token: can't save to the Keychain".to_string(),
    });
    let next = reclassify_refreshed_keychain_failure(orig).expect("some infra");
    assert_eq!(next.kind, InfraKind::Network);
    assert!(next.raw.contains(OAUTH_REFRESHED_MARKER));
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
fn host_refresh_minimal_seatbelt_isolates_writes_to_empty_cwd_and_keychain_only() {
    use std::fs;
    use std::process::Command;

    use aaf_engine::executors::macos_oauth_refresh::host_refresh_seatbelt_profile;

    let tmp = tempfile::tempdir().expect("tempdir");
    let empty_cwd = tmp.path().join("cwd");
    let kc_dir = tmp.path().join("keychains");
    let data_dir = tmp.path().join("data");
    let ssh_dir = tmp.path().join("ssh");
    let task_cwd = tmp.path().join("task");
    let other_dir = tmp.path().join("other");
    for d in [
        &empty_cwd, &kc_dir, &data_dir, &ssh_dir, &task_cwd, &other_dir,
    ] {
        fs::create_dir_all(d).expect("mkdir");
    }
    fs::write(data_dir.join("token"), "secret").expect("seed data");
    fs::write(ssh_dir.join("id_ed25519"), "secret").expect("seed ssh");
    fs::write(task_cwd.join("repo.rs"), "fn main() {}").expect("seed task");

    let profile = host_refresh_seatbelt_profile(
        &empty_cwd,
        &kc_dir,
        Some(&data_dir),
        Some(&ssh_dir),
        Some(&task_cwd),
    )
    .expect("profile");

    let run_sh = |script: &str, arg: &std::path::Path| -> bool {
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

    assert!(run_sh("printf ok > \"$1\"", &empty_cwd.join("ok")));
    assert!(run_sh("printf ok > \"$1\"", &kc_dir.join("ok")));
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
