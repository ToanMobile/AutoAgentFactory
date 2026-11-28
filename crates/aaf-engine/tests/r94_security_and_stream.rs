#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for build #94 security, Seatbelt root guarding, and Agy
//! JSON/error-classification hardening (P0-1, P0-2, P1-3, P1-10, P2-4, P2-5).

use aaf_domain::ports::InfraKind;
use aaf_engine::executors::agy::parse_agy_output;

#[test]
fn agy_sqlite_race_errors_classify_only_on_error_and_empty_error_string_is_not_error() {
    let opener_err = r#"{"conversation_id":"","status":"ERROR","response":"","error":"database was initialized by another opener"}"#;
    let p_err = parse_agy_output(opener_err, false);
    assert!(p_err.is_error);
    assert_eq!(
        p_err.infra.as_ref().map(|i| i.kind),
        Some(InfraKind::Network)
    );

    // Benign SUCCESS response prose mentioning both needles must NOT false-positive.
    let benign_ok = r#"{"conversation_id":"c1","status":"SUCCESS","error":"","response":"Fixed backend returned an empty conversation id and database was initialized by another opener","usage":{"input_tokens":50,"output_tokens":20}}"#;
    let p_ok = parse_agy_output(benign_ok, false);
    assert!(!p_ok.is_error, "empty error string must not force is_error");
    assert_eq!(p_ok.infra, None);
    assert_eq!(p_ok.limit_hit, None);
}

#[test]
fn agy_pretty_printed_json_prose_does_not_leak_into_auth_or_network_classifier() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;

    use aaf_domain::ports::{AgentRequest, AgentRunner, SandboxPolicy};
    use aaf_engine::executors::ExecEnv;
    use aaf_engine::executors::agy::AgyRunner;

    let tmp = tempfile::tempdir().unwrap();
    let script = tmp.path().join("fake-agy");
    // Emits exit code 1 with pretty-printed multi-line JSON whose `"response"`
    // quotes keyring/network phrases (and NO usage object). Must NOT misclassify
    // prose inside `"response"` as Auth or Network infra.
    fs::write(
        &script,
        "#!/bin/sh\ncat <<'EOF'\n{\n  \"status\": \"SUCCESS\",\n  \"response\": \"Reviewed Failed keyring SaveToken and database was initialized by another opener\"\n}\nEOF\nexit 1\n",
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

    let runner = AgyRunner::new(script.to_string_lossy(), ExecEnv::default());
    let out = runner
        .run(&AgentRequest {
            run_id: "test-pretty-json".into(),
            prompt: "review".into(),
            cwd: tmp.path().to_string_lossy().into_owned(),
            account: None,
            timeout_secs: 15,
            max_turns: None,
            sandbox: SandboxPolicy::default(),
            json_schema: None,
        })
        .unwrap();
    assert_eq!(out.exit_code, Some(1));
    assert_eq!(
        out.infra, None,
        "multi-line JSON response prose must never trigger auth/network infra"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn agy_runtime_writable_subdirs_block_two_run_seatbelt_symlink_escape() {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::process::Command;

    use aaf_domain::ports::{AgentRequest, PortError, SandboxPolicy};
    use aaf_engine::executors::ExecEnv;
    use aaf_engine::executors::agy::AgyRunner;
    use aaf_engine::executors::sandbox::{SandboxPaths, seatbelt_profile};

    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().canonicalize().unwrap();
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
        run_id: "r94-agy-escape".into(),
        prompt: "inspect".into(),
        cwd: home.to_string_lossy().into_owned(),
        account: None,
        timeout_secs: 30,
        max_turns: None,
        sandbox: policy.clone(),
        json_schema: None,
    };
    let acct = runner.account_paths(&req).unwrap();
    let profile = seatbelt_profile(
        &policy,
        &SandboxPaths {
            writable: acct.writable,
            writable_regex: acct.writable_regex,
            write_denied: acct.write_denied,
            ..SandboxPaths::default()
        },
    )
    .unwrap();

    let agy_tmp = home.join(".gemini/antigravity/tmp");
    // Confined process CAN write files inside tmp/, CANNOT rm -rf tmp && ln -s $HOME tmp.
    let ok_inside = Command::new("/usr/bin/sandbox-exec")
        .args([
            "-p",
            &profile,
            "--",
            "/bin/sh",
            "-c",
            "printf ok > \"$1/child\"",
            "_",
            &agy_tmp.to_string_lossy(),
        ])
        .status()
        .unwrap()
        .success();
    assert!(ok_inside, "writing inside runtime tmp dir must still work");

    let escape_ok = Command::new("/usr/bin/sandbox-exec")
        .args([
            "-p",
            &profile,
            "--",
            "/bin/sh",
            "-c",
            "rm -rf \"$1\" && ln -s \"$2\" \"$1\"",
            "_",
            &agy_tmp.to_string_lossy(),
            &home.to_string_lossy(),
        ])
        .status()
        .unwrap()
        .success();
    assert!(
        !escape_ok,
        "confined process must NEVER unlink/replace AGY_RUNTIME_WRITABLE_SUBDIRS root"
    );

    // Even if an external actor pre-planted a symlink at antigravity/tmp,
    // account_paths must fail closed with PortError::Invalid.
    fs::remove_dir_all(&agy_tmp).unwrap();
    symlink(&home, &agy_tmp).unwrap();
    assert!(
        matches!(runner.account_paths(&req), Err(PortError::Invalid(_))),
        "pre-planted symlink inside .gemini runtime subdir must fail closed"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn claude_session_state_roots_cannot_be_unlinked_or_swapped_inside_seatbelt() {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    use aaf_domain::account::{AccountProfile, ProviderKind};
    use aaf_domain::ports::{AgentRequest, AgentRunner, SandboxPolicy};
    use aaf_engine::executors::ExecEnv;
    use aaf_engine::executors::claude::ClaudeCodeRunner;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let home = root.join("home");
    let shared = home.join(".claude");
    let projects = shared.join("projects");
    let history = shared.join("history.jsonl");
    let acct = root.join("acct");
    let wt = root.join("wt");
    for d in [&projects, &acct, &wt] {
        fs::create_dir_all(d).unwrap();
    }
    fs::write(&history, "{}\n").unwrap();
    symlink(&projects, acct.join("projects")).unwrap();
    symlink(&history, acct.join("history.jsonl")).unwrap();

    let claude = root.join("claude");
    fs::write(
        &claude,
        format!(
            "#!/bin/sh\n\
             (printf ok > \"{p}/child\" 2>/dev/null && echo child-ok) || echo child-fail\n\
             (printf line >> \"{h}\" 2>/dev/null && echo hist-write-ok) || echo hist-write-fail\n\
             (rm -rf \"{p}\" 2>/dev/null && echo proj-rm-pwn) || echo proj-rm-denied\n\
             (rm -f \"{h}\" 2>/dev/null && echo hist-rm-pwn) || echo hist-rm-denied\n",
            p = projects.display(),
            h = history.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();

    let env = ExecEnv {
        home: Some(home.to_string_lossy().into_owned()),
        temp_dirs: Vec::new(),
        ..ExecEnv::default()
    };
    let out = ClaudeCodeRunner::new(claude.to_string_lossy(), env)
        .run(&AgentRequest {
            run_id: "r94-session-guard".into(),
            prompt: "hi".into(),
            cwd: wt.to_string_lossy().into_owned(),
            account: Some(AccountProfile {
                id: "claude_code-guard".into(),
                provider: ProviderKind::ClaudeCode,
                config_dir: acct.to_string_lossy().into_owned(),
                name: "guard".into(),
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
        .unwrap();
    for marker in [
        "child-ok",
        "hist-write-ok",
        "proj-rm-denied",
        "hist-rm-denied",
    ] {
        assert!(
            out.log_tail.contains(marker),
            "expected {marker} in tail: {}",
            out.log_tail
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn host_refresh_seatbelt_rejects_lock_symlinks_and_sibling_lock_overlap() {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::process::Command;

    use aaf_engine::executors::macos_oauth_refresh::host_refresh_seatbelt_profile;

    let tmp = tempfile::tempdir().unwrap();
    let empty_cwd = tmp.path().join("empty");
    let kc_dir = tmp.path().join("kc");
    let claude_cfg = tmp.path().join("claude-cfg");
    let sibling_lock = tmp.path().join("claude-cfg.lock");
    let symlink_cfg = tmp.path().join("claude-link");
    for d in [&empty_cwd, &kc_dir, &claude_cfg, &sibling_lock] {
        fs::create_dir_all(d).unwrap();
    }
    fs::write(claude_cfg.join("settings.json"), "{}").unwrap();
    symlink(&claude_cfg, &symlink_cfg).unwrap();

    // 1. Sibling `{cfg}.lock` or `keychain_dir` overlapping denied_dirs must fail closed.
    assert!(
        host_refresh_seatbelt_profile(&empty_cwd, &kc_dir, Some(&claude_cfg), &[&sibling_lock])
            .is_none(),
        "sibling .lock overlapping denied dir must fail closed"
    );
    assert!(
        host_refresh_seatbelt_profile(&empty_cwd, tmp.path(), None, &[&claude_cfg]).is_none(),
        "keychain_dir acting as ancestor of a denied dir must fail closed"
    );
    // 2. Symlinked claude_config_dir pointing at a denied dir must fail closed,
    //    while a dotfiles symlink pointing at a non-denied dir succeeds.
    assert!(
        host_refresh_seatbelt_profile(&empty_cwd, &kc_dir, Some(&symlink_cfg), &[&claude_cfg])
            .is_none(),
        "symlinked claude_config_dir targeting denied dir must fail closed"
    );
    assert!(
        host_refresh_seatbelt_profile(&empty_cwd, &kc_dir, Some(&symlink_cfg), &[]).is_some(),
        "dotfiles symlinked claude_config_dir targeting non-denied dir must work"
    );

    // 3. Symlink planted at .oauth_refresh.lock -> settings.json cannot bypass Seatbelt.
    let profile =
        host_refresh_seatbelt_profile(&empty_cwd, &kc_dir, Some(&claude_cfg), &[]).unwrap();
    symlink(
        claude_cfg.join("settings.json"),
        claude_cfg.join(".oauth_refresh.lock"),
    )
    .unwrap();
    let pwn = Command::new("/usr/bin/sandbox-exec")
        .args([
            "-p",
            &profile,
            "--",
            "/bin/sh",
            "-c",
            "printf pwn > \"$1\"",
            "_",
            &claude_cfg.join(".oauth_refresh.lock").to_string_lossy(),
        ])
        .status()
        .unwrap()
        .success();
    assert!(
        !pwn,
        "symlinked .oauth_refresh.lock must not write settings.json"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn host_refresh_command_profile_succeeds_for_claude_and_denies_plugins_and_hooks() {
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use aaf_engine::executors::ExecEnv;
    use aaf_engine::executors::macos_oauth_refresh::host_refresh_command_profile;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let home = root.join("home");
    let kc = home.join("Library/Keychains");
    let data_dir = root.join("data");
    let empty_cwd = data_dir.join("oauth-refresh-tmp/run-1");
    let task_cwd = root.join("task");
    let claude_cfg = home.join(".claude-finos");
    let plugins = claude_cfg.join("plugins");
    let hooks = claude_cfg.join("hooks");
    for d in [&kc, &empty_cwd, &task_cwd, &plugins, &hooks] {
        fs::create_dir_all(d).unwrap();
    }
    fs::write(plugins.join("p.sh"), "secret-plugin").unwrap();
    fs::write(hooks.join("h.sh"), "secret-hook").unwrap();
    fs::write(claude_cfg.join("settings.json"), "{}").unwrap();

    let env = ExecEnv {
        home: Some(home.to_string_lossy().into_owned()),
        data_dir: Some(data_dir.to_string_lossy().into_owned()),
        ..ExecEnv::default()
    };
    let profile = host_refresh_command_profile(
        &env,
        &home,
        &task_cwd.to_string_lossy(),
        &empty_cwd,
        Some(&claude_cfg),
    )
    .expect("build_host_sandbox_profile must return Some for real claude_config_dir");

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
            .unwrap()
            .success()
    };
    assert!(!run_sh("cat \"$1\" >/dev/null 2>&1", &plugins.join("p.sh")));
    assert!(!run_sh("printf bad > \"$1\"", &plugins.join("p.sh")));
    assert!(!run_sh("cat \"$1\" >/dev/null 2>&1", &hooks.join("h.sh")));
    assert!(!run_sh("printf bad > \"$1\"", &hooks.join("h.sh")));
    assert!(!run_sh(
        "printf bad > \"$1\"",
        &claude_cfg.join("settings.json")
    ));
    assert!(run_sh(
        "mkdir \"$1\" && rmdir \"$1\"",
        &claude_cfg.join(".oauth_refresh.lock")
    ));
    assert!(run_sh(
        "mkdir \"$1\" && rmdir \"$1\"",
        &home.join(".claude-finos.lock")
    ));
}

#[cfg(target_os = "macos")]
#[test]
fn write_denied_prefix_files_and_symlink_targets_stay_unwritable_under_seatbelt() {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::process::Command;

    use aaf_domain::ports::SandboxPolicy;
    use aaf_engine::executors::sandbox::{SandboxPaths, seatbelt_profile};

    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().canonicalize().unwrap();
    let cargo = home.join(".cargo");
    let gradle = home.join(".gradle");
    fs::create_dir_all(&cargo).unwrap();
    fs::create_dir_all(&gradle).unwrap();
    let cargo_cfg = cargo.join("config");
    let cargo_real = cargo.join("real_config");
    let cargo_toml_link = cargo.join("config.toml");
    let gradle_init = gradle.join("init.gradle");
    fs::write(&cargo_cfg, "[build]\n").unwrap();
    fs::write(&cargo_real, "[build]\n").unwrap();
    fs::write(&gradle_init, "// init\n").unwrap();
    symlink(&cargo_real, &cargo_toml_link).unwrap();

    let policy = SandboxPolicy {
        enabled: true,
        read_only: false,
        ..SandboxPolicy::default()
    };
    let profile = seatbelt_profile(
        &policy,
        &SandboxPaths {
            writable: vec![
                cargo.to_string_lossy().into_owned(),
                gradle.to_string_lossy().into_owned(),
            ],
            write_denied_prefix: vec![
                cargo_cfg.to_string_lossy().into_owned(),
                gradle_init.to_string_lossy().into_owned(),
            ],
            ..SandboxPaths::default()
        },
    )
    .unwrap();

    for target in [&cargo_cfg, &cargo_real, &cargo_toml_link, &gradle_init] {
        let ok = Command::new("/usr/bin/sandbox-exec")
            .args([
                "-p",
                &profile,
                "--",
                "/bin/sh",
                "-c",
                "printf pwn >> \"$1\"",
                "_",
                &target.to_string_lossy(),
            ])
            .status()
            .unwrap()
            .success();
        assert!(
            !ok,
            "write_denied_prefix file or symlink target must stay strictly write-denied: {}",
            target.display()
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn symlinked_build_cache_dir_is_skipped_and_never_opens_home_on_next_run() {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    use aaf_domain::ports::{AgentRequest, AgentRunner, SandboxPolicy};
    use aaf_engine::executors::ExecEnv;
    use aaf_engine::executors::claude::ClaudeCodeRunner;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let home = root.join("home");
    let wt = root.join("wt");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&wt).unwrap();
    // Simulate a hostile Run 1 that planted:
    //  - `~/.konan -> $HOME` (missing BUILD_CACHE_DIRS)
    //  - `~/.claude.json -> $HOME/victim`
    //  - `~/.my-rustup -> $HOME` with trailing `/` in `RUSTUP_HOME` (POSIX lstat bypass)
    //  - `~/.cache/tools -> $HOME` with intermediate symlink `GRADLE_USER_HOME=~/.cache/tools/secret_dir`
    fs::write(home.join("victim"), "clean").unwrap();
    fs::create_dir_all(home.join(".cache")).unwrap();
    fs::create_dir_all(home.join("secret_dir")).unwrap();
    symlink(&home, home.join(".konan")).unwrap();
    symlink(home.join("victim"), home.join(".claude.json")).unwrap();
    symlink(&home, home.join(".my-rustup")).unwrap();
    symlink(&home, home.join(".cache/tools")).unwrap();

    let claude = root.join("claude");
    fs::write(
        &claude,
        format!(
            "#!/bin/sh\n\
             (printf pwn > \"{h}/escaped\" 2>/dev/null && echo home-pwn) || echo home-denied\n\
             (printf pwn > \"{h}/victim\" 2>/dev/null && echo victim-pwn) || echo victim-denied\n\
             (printf pwn > \"{h}/secret_dir/pwn\" 2>/dev/null && echo inter-pwn) || echo inter-denied\n",
            h = home.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();

    let env = ExecEnv {
        home: Some(home.to_string_lossy().into_owned()),
        rustup_home: Some(format!("{}/.my-rustup/", home.display())),
        gradle_user_home: Some(format!("{}/.cache/tools/secret_dir", home.display())),
        temp_dirs: Vec::new(),
        ..ExecEnv::default()
    };
    let out = ClaudeCodeRunner::new(claude.to_string_lossy(), env)
        .run(&AgentRequest {
            run_id: "r94-symlink-cache".into(),
            prompt: "hi".into(),
            cwd: wt.to_string_lossy().into_owned(),
            account: None,
            timeout_secs: 30,
            max_turns: None,
            sandbox: SandboxPolicy {
                enabled: true,
                read_only: false,
                ..SandboxPolicy::default()
            },
            json_schema: None,
        })
        .unwrap();
    assert!(out.log_tail.contains("home-denied"), "{}", out.log_tail);
    assert!(out.log_tail.contains("victim-denied"), "{}", out.log_tail);
    assert!(out.log_tail.contains("inter-denied"), "{}", out.log_tail);
    assert!(!home.join("escaped").exists());
    assert!(!home.join("secret_dir/pwn").exists());
    assert_eq!(fs::read_to_string(home.join("victim")).unwrap(), "clean");
}

#[test]
fn agy_false_and_whitespace_error_fields_are_not_errors_and_claude_combines_result_and_errors() {
    use aaf_engine::executors::claude::parse_stream;

    let false_err = r#"{"status":"SUCCESS","error":false,"response":"all good"}"#;
    assert!(!parse_agy_output(false_err, false).is_error);
    let ws_err = r#"{"status":"SUCCESS","error":"   ","response":"all good"}"#;
    assert!(!parse_agy_output(ws_err, false).is_error);

    // Numeric HTTP 401/429 JSON error & multi-JSON with trailing diagnostic object.
    let num_401 = r#"API error: {"status":401,"message":"You are not logged into Antigravity."}"#;
    let p401 = parse_agy_output(num_401, false);
    assert!(p401.is_error && p401.infra.as_ref().map(|i| i.kind) == Some(InfraKind::Auth));
    let multi =
        "{\"status\":\"ERROR\",\"error\":\"Failed keyring SaveToken\"}\n{\"metrics\":{\"ms\":12}}";
    let pm = parse_agy_output(multi, false);
    assert!(pm.is_error && pm.infra.as_ref().map(|i| i.kind) == Some(InfraKind::Auth));

    // Claude stream-json error_during_execution with BOTH non-empty result and errors[]
    // preserves network infra AND rate-limit detection + reset_at from errors[].
    let stream = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"execution failed","errors":["Connection error: ECONNRESET while calling API"]}"#;
    let summary = parse_stream(stream);
    assert!(summary.is_error && summary.infra.as_ref().map(|i| i.kind) == Some(InfraKind::Network));
    let lim_stream = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"execution failed","errors":["Usage limit reached|1795800000"]}"#;
    let lim = parse_stream(lim_stream);
    assert!(lim.is_error && lim.limit_hit.is_some());
    assert_eq!(lim.limit_hit.unwrap().reset_at, Some(1_795_800_000));
}
