#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Wave 1 RED->GREEN oracles for `AgyRunner`, `ClaudeCodeRunner` `--json-schema`
//! & `structured_output`, `GenericRunner` `~/.gemini` narrowing, registry
//! `"antigravity"` arm, and cross-provider sandbox isolation (R3-SEC-1/2/9, R3-PROTO-1).

use std::ffi::OsString;
use std::fs;
use std::path::Path;

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::config::RunConfig;
use aaf_domain::ports::{AgentRequest, InfraKind, PortError, SandboxPolicy};
use aaf_engine::executors::agy::{
    AGY_RUNTIME_WRITABLE_SUBDIRS, AgyRunner, GEMINI_CONFIG_CLOSED, parse_agy_output,
};
use aaf_engine::executors::claude::{ClaudeCodeRunner, parse_stream};
use aaf_engine::executors::generic;
use aaf_engine::executors::linux_sandbox::build_bwrap_args;
use aaf_engine::executors::registry::{build_headless_argv, find_descriptor};
use aaf_engine::executors::sandbox::{SandboxPaths, seatbelt_profile};
use aaf_engine::executors::{ExecEnv, build_runner_with};
use aaf_engine::factory::prompt::sandbox_policy_for;

fn base_request(cwd: &Path, sandbox: SandboxPolicy) -> AgentRequest {
    AgentRequest {
        run_id: "w1-test-run".to_string(),
        prompt: "inspect plan".to_string(),
        cwd: cwd.to_string_lossy().into_owned(),
        account: None,
        timeout_secs: 60,
        sandbox,
        json_schema: None,
    }
}

fn account(provider: ProviderKind, config_dir: &Path) -> AccountProfile {
    AccountProfile {
        id: format!("{}-acct", provider.as_str()),
        provider,
        config_dir: config_dir.to_string_lossy().into_owned(),
        name: format!("{}-acct", provider.as_str()),
    }
}

#[test]
fn claude_parse_stream_reads_structured_output_and_max_retries_error() {
    let ok_line = r#"{"type":"result","subtype":"success","is_error":false,"result":"","structured_output":{"verdict":"CONVERGED","count":2},"usage":{"input_tokens":100,"output_tokens":25}}"#;
    let summary = parse_stream(ok_line);
    assert!(!summary.is_error);
    let parsed: serde_json::Value = serde_json::from_str(&summary.final_text).unwrap();
    assert_eq!(parsed["verdict"], "CONVERGED");
    assert_eq!(parsed["count"], 2);
    assert_eq!((summary.tokens_in, summary.tokens_out), (100, 25));

    let fallback_line =
        r#"{"type":"result","is_error":false,"result":"plain fallback","structured_output":null}"#;
    assert_eq!(parse_stream(fallback_line).final_text, "plain fallback");

    let retry_err_line = r#"{"type":"result","subtype":"error_max_structured_output_retries","is_error":false,"result":""}"#;
    let err_summary = parse_stream(retry_err_line);
    assert!(
        err_summary.is_error,
        "max retries subtype must force is_error"
    );
}

#[test]
fn claude_command_passes_json_schema_before_double_dash() {
    let tmp = tempfile::tempdir().unwrap();
    let runner = ClaudeCodeRunner::new("/usr/bin/claude", ExecEnv::default());
    let mut req = base_request(tmp.path(), SandboxPolicy::default());
    req.json_schema = Some(r#"{"type":"object"}"#.to_string());
    let spec = runner.command(&req).unwrap();
    let schema_pos = spec.args.iter().position(|a| a == "--json-schema").unwrap();
    let dash_pos = spec.args.iter().position(|a| a == "--").unwrap();
    assert_eq!(spec.args[schema_pos + 1], r#"{"type":"object"}"#);
    assert!(schema_pos < dash_pos);
    assert_eq!(spec.args[dash_pos + 1], req.prompt);
}

#[test]
fn agy_command_three_way_mode_flags_and_validation() {
    let tmp = tempfile::tempdir().unwrap();
    let runner = AgyRunner::new("/usr/local/bin/agy", ExecEnv::default())
        .with_model_and_effort("test-model", "HIGH");

    // 1. read_only -> --mode plan (no skip-permissions)
    let mut req = base_request(
        tmp.path(),
        SandboxPolicy {
            enabled: true,
            read_only: true,
            ..SandboxPolicy::default()
        },
    );
    req.json_schema = Some(r#"{"type":"object"}"#.to_string());
    let plan_spec = runner.command(&req).unwrap();
    assert_eq!(
        plan_spec.args,
        vec![
            "--output-format",
            "json",
            "--disable-slash-commands",
            "--mode",
            "plan",
            "--model",
            "test-model",
            "--effort",
            "high",
            "--json-schema",
            r#"{"type":"object"}"#,
            "-p=inspect plan",
        ]
    );
    assert!(
        !plan_spec
            .args
            .iter()
            .any(|a| a == "--dangerously-skip-permissions")
    );

    // 2. writable + sandboxed -> --mode auto --dangerously-skip-permissions
    req.sandbox = SandboxPolicy {
        enabled: true,
        read_only: false,
        ..SandboxPolicy::default()
    };
    req.json_schema = None;
    let auto_spec = runner.command(&req).unwrap();
    assert!(auto_spec.args.windows(2).any(|w| w == ["--mode", "auto"]));
    assert!(
        auto_spec
            .args
            .iter()
            .any(|a| a == "--dangerously-skip-permissions")
    );

    // 3. writable + unsandboxed -> --mode accept-edits (NEVER skip-permissions)
    req.sandbox = SandboxPolicy {
        enabled: false,
        read_only: false,
        ..SandboxPolicy::default()
    };
    let accept_spec = runner.command(&req).unwrap();
    assert!(
        accept_spec
            .args
            .windows(2)
            .any(|w| w == ["--mode", "accept-edits"])
    );
    assert!(
        !accept_spec
            .args
            .iter()
            .any(|a| a == "--dangerously-skip-permissions")
    );

    // 4. rejects non-Antigravity account
    req.account = Some(account(ProviderKind::ClaudeCode, tmp.path()));
    assert!(matches!(runner.command(&req), Err(PortError::Invalid(_))));
    assert!(matches!(
        runner.account_paths(&req),
        Err(PortError::Invalid(_))
    ));

    // 5. rejects bad model / effort
    req.account = None;
    let bad_model =
        AgyRunner::new("/bin/agy", ExecEnv::default()).with_model_and_effort("--bad", "high");
    assert!(matches!(
        bad_model.command(&req),
        Err(PortError::Invalid(_))
    ));
    let bad_effort =
        AgyRunner::new("/bin/agy", ExecEnv::default()).with_model_and_effort("m", "ultra");
    assert!(matches!(
        bad_effort.command(&req),
        Err(PortError::Invalid(_))
    ));
}

#[test]
fn agy_resolve_binary_and_build_runner_routing() {
    let dir = tempfile::tempdir().unwrap();
    let bin_dir = dir.path().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let fallback = bin_dir.join("antigravity");
    fs::write(&fallback, "#!/bin/sh\n").unwrap();
    let path_var: OsString = std::env::join_paths([bin_dir.as_path()]).unwrap();
    assert_eq!(AgyRunner::resolve_binary(&path_var), Some(fallback));

    let primary = bin_dir.join("agy");
    fs::write(&primary, "#!/bin/sh\n").unwrap();
    assert_eq!(AgyRunner::resolve_binary(&path_var), Some(primary.clone()));

    let mut cfg = RunConfig {
        executor: "antigravity".to_string(),
        ..RunConfig::default()
    };
    let env = ExecEnv {
        agy_bin: Some(primary.to_string_lossy().into_owned()),
        ..ExecEnv::default()
    };
    let native = build_runner_with(&cfg, env.clone()).unwrap();
    assert!(native.reports_tokens(), "native AgyRunner reports tokens");

    cfg.executor_cmd = vec!["/bin/echo".to_string(), "{prompt}".to_string()];
    let custom = build_runner_with(&cfg, env).unwrap();
    assert!(
        !custom.reports_tokens(),
        "explicit executor_cmd falls back to GenericRunner"
    );
}

#[test]
fn agy_parse_output_structured_usage_and_error_classification() {
    let stdout = "\x1b[32minfo: starting\x1b[0m\n{\n  \"response\": \"ignored\",\n  \"structured_output\": {\"ok\": true, \"items\": [1]},\n  \"usage\": {\"input_tokens\": 1200, \"cache_read_tokens\": 400, \"output_tokens\": 85}\n}\n[aaf] done\n";
    let parsed = parse_agy_output(stdout, true);
    assert!(!parsed.is_error);
    assert!(parsed.usage_present);
    assert_eq!((parsed.tokens_in, parsed.tokens_out), (800, 85));
    let val: serde_json::Value = serde_json::from_str(&parsed.final_text).unwrap();
    assert_eq!(val["ok"], true);
    assert!(parsed.limit_hit.is_none() && parsed.infra.is_none());

    // Normal text mentioning 429 must NOT trigger LimitHit when !is_error.
    let ok_429 = r#"{"response":"Handles HTTP 429 retries cleanly","usage":{"input_tokens":10,"output_tokens":5}}"#;
    let parsed_ok = parse_agy_output(ok_429, false);
    assert!(!parsed_ok.is_error && parsed_ok.limit_hit.is_none() && parsed_ok.infra.is_none());

    // 429 / RESOURCE_EXHAUSTED -> LimitHit
    let rate = parse_agy_output(
        r#"{"is_error":true,"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"quota exceeded"}}"#,
        false,
    );
    assert!(rate.is_error && rate.limit_hit.is_some() && rate.infra.is_none());

    // 401 / login required -> InfraKind::Auth
    let auth = parse_agy_output("Error: HTTP 401 unauthenticated - login required", false);
    assert_eq!(auth.infra.as_ref().map(|i| i.kind), Some(InfraKind::Auth));

    // DNS / connection refused -> InfraKind::Network
    let net = parse_agy_output(
        "dial tcp: lookup api.example: no such host (connection refused)",
        false,
    );
    assert_eq!(net.infra.as_ref().map(|i| i.kind), Some(InfraKind::Network));
}

#[test]
fn agy_account_paths_precreates_runtime_dirs_and_protects_config() {
    let tmp = tempfile::tempdir().unwrap();
    let env = ExecEnv {
        home: Some(tmp.path().to_string_lossy().into_owned()),
        ..ExecEnv::default()
    };
    let runner = AgyRunner::new("/bin/agy", env);
    let req = base_request(
        tmp.path(),
        SandboxPolicy {
            enabled: true,
            read_only: true,
            ..SandboxPolicy::default()
        },
    );
    // account == None must still derive $HOME/.gemini and create all 6 subdirs.
    let paths = runner.account_paths(&req).unwrap();
    let gemini = tmp.path().join(".gemini");
    for rel in AGY_RUNTIME_WRITABLE_SUBDIRS {
        assert!(gemini.join(rel).is_dir(), "expected created subdir {rel}");
        assert!(paths.writable.iter().any(|p| p.ends_with(rel)));
    }
    assert!(
        paths.writable.iter().all(|p| !p.contains('*')),
        "no globs in writable"
    );
    for closed in GEMINI_CONFIG_CLOSED {
        let expected = format!("{}/{closed}", gemini.display());
        assert!(
            paths.write_denied.contains(&expected),
            "missing write_denied {expected}"
        );
    }
    assert_eq!(paths.writable_regex.len(), 1);
    assert!(
        paths.writable_regex[0]
            .ends_with(r"/antigravity/conversation_summaries\.db(-wal|-shm|-journal)?$")
    );

    // SBPL emits writable_regex BEFORE the deny section and rejects empty patterns.
    let extra = SandboxPaths {
        writable: paths.writable,
        writable_regex: paths.writable_regex,
        write_denied: paths.write_denied,
        ..SandboxPaths::default()
    };
    let profile = seatbelt_profile(&req.sandbox, &extra).unwrap();
    let allow_pos = profile.find("conversation_summaries").unwrap();
    let deny_pos = profile.find("settings.json").unwrap();
    assert!(
        allow_pos < deny_pos,
        "writable_regex must precede denies:\n{profile}"
    );

    let bad = SandboxPaths {
        writable_regex: vec![String::new()],
        ..SandboxPaths::default()
    };
    assert!(matches!(
        seatbelt_profile(&req.sandbox, &bad),
        Err(PortError::Invalid(_))
    ));
}

#[cfg(target_os = "macos")]
#[test]
fn live_seatbelt_smoke_test_enforces_agy_gemini_confinement() {
    use std::process::Command;

    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let gemini = home.join(".gemini");
    fs::create_dir_all(gemini.join("antigravity/mcp")).unwrap();
    fs::create_dir_all(gemini.join("antigravity/bin")).unwrap();
    fs::write(gemini.join("settings.json"), "{}").unwrap();
    fs::write(gemini.join("trusted_hooks.json"), "{}").unwrap();

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
    let req = base_request(home, policy.clone());
    let acct = runner.account_paths(&req).unwrap();
    let extra = SandboxPaths {
        writable: acct.writable,
        writable_regex: acct.writable_regex,
        write_denied: acct.write_denied,
        ..SandboxPaths::default()
    };
    let profile = seatbelt_profile(&policy, &extra).unwrap();

    let try_write = |target: &Path| -> bool {
        Command::new("/usr/bin/sandbox-exec")
            .args([
                "-p",
                &profile,
                "--",
                "/bin/sh",
                "-c",
                "printf ok > \"$1\"",
                "_",
                target.to_str().unwrap(),
            ])
            .status()
            .unwrap()
            .success()
    };

    // Allowed runtime subdirs + conversation_summaries.db and WAL sidecars.
    assert!(try_write(
        &gemini.join("antigravity/conversations/turn1.json")
    ));
    assert!(try_write(
        &gemini.join("antigravity/conversation_summaries.db")
    ));
    assert!(try_write(
        &gemini.join("antigravity/conversation_summaries.db-wal")
    ));

    // Denied config/hook/plugin entries and arbitrary sibling files.
    assert!(!try_write(&gemini.join("settings.json")));
    assert!(!try_write(&gemini.join("trusted_hooks.json")));
    assert!(!try_write(&gemini.join("antigravity/mcp/evil.json")));
    assert!(!try_write(&gemini.join("antigravity/bin/evil")));
    assert!(!try_write(&gemini.join("antigravity/other.txt")));
}

#[test]
fn generic_runner_antigravity_narrows_writable_and_closes_config() {
    let tmp = tempfile::tempdir().unwrap();
    let gemini = tmp.path().join(".gemini");
    let mut req = base_request(
        tmp.path(),
        SandboxPolicy {
            enabled: true,
            read_only: false,
            ..SandboxPolicy::default()
        },
    );
    req.account = Some(account(ProviderKind::Antigravity, &gemini));
    let paths = generic::account_paths(&req);
    assert_eq!(
        paths.writable,
        vec![
            gemini.join("tmp").to_string_lossy().into_owned(),
            gemini.join("history").to_string_lossy().into_owned(),
        ]
    );
    assert!(
        !paths
            .writable
            .contains(&gemini.to_string_lossy().into_owned())
    );
    assert!(gemini.join("tmp").is_dir() && gemini.join("history").is_dir());
    for closed in GEMINI_CONFIG_CLOSED {
        assert!(
            paths
                .write_denied
                .contains(&format!("{}/{closed}", gemini.display()))
        );
    }
}

#[test]
fn registry_antigravity_descriptor_and_headless_argv() {
    let desc = find_descriptor("antigravity").unwrap();
    assert!(desc.supports_model_flag);
    assert!(desc.supports_effort_flag);
    assert!(desc.supports_read_only_flag);
    let argv = build_headless_argv(
        desc,
        "/usr/local/bin/agy",
        "review diff",
        Some("test-model"),
        Some("high"),
        true,
    )
    .unwrap();
    assert_eq!(
        argv,
        vec![
            "/usr/local/bin/agy",
            "--disable-slash-commands",
            "--mode",
            "plan",
            "--model",
            "test-model",
            "--effort",
            "high",
            "-p=review diff",
        ]
    );
}

#[test]
fn sandbox_policy_enforces_cross_provider_isolation_and_linux_gemini_ro() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let data = tmp.path().join("data");
    let real_gemini = tmp.path().join("real-gemini");
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::write(home.join(".claude.json"), "{}").unwrap();
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&real_gemini).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real_gemini, home.join(".gemini")).unwrap();
    #[cfg(not(unix))]
    fs::create_dir_all(home.join(".gemini")).unwrap();

    // Antigravity run denies ~/.claude and ~/.claude.json even when `all` is empty.
    let agy_policy = sandbox_policy_for(true, &data, &home, ProviderKind::Antigravity, None, &[]);
    assert!(
        agy_policy
            .read_denied_paths
            .contains(&home.join(".claude").to_string_lossy().into_owned())
    );
    assert!(
        agy_policy
            .read_denied_paths
            .contains(&home.join(".claude.json").to_string_lossy().into_owned())
    );
    assert!(
        !agy_policy
            .read_denied_paths
            .contains(&home.join(".gemini").to_string_lossy().into_owned())
    );

    // Claude run denies ~/.gemini (even as symlink & absent from `all`), keeps ~/.claude open.
    let claude_policy = sandbox_policy_for(true, &data, &home, ProviderKind::ClaudeCode, None, &[]);
    assert!(
        claude_policy
            .read_denied_paths
            .contains(&home.join(".gemini").to_string_lossy().into_owned())
    );
    assert!(
        !claude_policy
            .read_denied_paths
            .contains(&home.join(".claude").to_string_lossy().into_owned())
    );

    // Linux bwrap locks worktree .gemini/settings.json and .gemini/trusted_hooks.json read-only.
    let wt = tmp.path().join("wt");
    fs::create_dir_all(wt.join(".gemini")).unwrap();
    let g_settings = wt.join(".gemini/settings.json");
    let g_hooks = wt.join(".gemini/trusted_hooks.json");
    fs::write(&g_settings, "{}").unwrap();
    fs::write(&g_hooks, "{}").unwrap();
    let sp = SandboxPaths {
        worktree: Some(wt.to_string_lossy().into_owned()),
        ..SandboxPaths::default()
    };
    let argv = build_bwrap_args(&sp, "/bin/sh", &["-c".to_string(), "true".to_string()]).unwrap();
    let has_ro = |p: &Path| {
        let s = p.to_string_lossy();
        argv.windows(3)
            .any(|w| w[0] == "--ro-bind" && w[1] == s && w[2] == s)
    };
    assert!(
        has_ro(&g_settings),
        "expected --ro-bind for .gemini/settings.json"
    );
    assert!(
        has_ro(&g_hooks),
        "expected --ro-bind for .gemini/trusted_hooks.json"
    );
}
