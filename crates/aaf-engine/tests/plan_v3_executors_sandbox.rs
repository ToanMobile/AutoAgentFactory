#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Plan v3 Wave 3 Slice 3 tests: Multi-CLI registry wiring, non-interactive PTY
//! env, Tier-1 `writable_caches` validation, non-Claude `write_denied` & quota/infra
//! parser, Linux `bwrap` RO cache roots, unsandboxed Claude explicit bypass,
//! ANSI stripping in `verdict.rs`, and `[cross_audit] executor = "auto"`.

use std::ffi::OsString;
use std::fs;
use std::path::Path;

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::config::{CrossAuditConfig, RunConfig};
use aaf_domain::ports::{AgentRequest, AgentRunner, InfraKind, SandboxPolicy};
use aaf_domain::task::ImmutableSpec;
use aaf_engine::cross_audit::{AuditVerdict, build_checklist, parse_verdict};
use aaf_engine::executors::claude::ClaudeCodeRunner;
use aaf_engine::executors::generic::{GenericRunner, classify_generic_failure};
use aaf_engine::executors::linux_sandbox::build_bwrap_args;
use aaf_engine::executors::registry::{
    build_headless_argv, build_headless_argv_confined, find_cli,
};
use aaf_engine::executors::sandbox::SandboxPaths;
use aaf_engine::executors::{
    BUILD_CACHE_DIRS, ExecEnv, agent_env, build_runner_with, validate_writable_caches,
};
use aaf_engine::factory::audit_backend::{build_audit_command, resolve_auto_audit_executor};
use tempfile::TempDir;

fn req(prompt: &str, cwd: &str) -> AgentRequest {
    AgentRequest {
        run_id: format!("p3-{}", uuid::Uuid::new_v4()),
        prompt: prompt.to_string(),
        cwd: cwd.to_string(),
        account: None,
        timeout_secs: 30,
        sandbox: SandboxPolicy::default(),
        json_schema: None,
    }
}

fn fake_bin(dir: &Path, name: &str) {
    let p = dir.join(name);
    fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn has2(argv: &[String], a: &str, b: &str) -> bool {
    argv.windows(2).any(|w| w == [a, b])
}

fn has3(argv: &[String], a: &str, b: &str, c: &str) -> bool {
    argv.windows(3).any(|w| w[0] == a && w[1] == b && w[2] == c)
}

#[test]
fn build_runner_with_resolves_builtin_clis_when_executor_cmd_is_empty() {
    let tmp = TempDir::new().unwrap();
    for bin in [
        "codex",
        "gemini",
        "opencode",
        "cursor-agent",
        "dsh",
        "codebuddy",
        "kimi",
        "pi",
    ] {
        fake_bin(tmp.path(), bin);
    }
    let path_str = tmp.path().to_string_lossy().into_owned();
    let env = ExecEnv {
        parent_env: vec![("PATH".to_string(), path_str)],
        ..ExecEnv::default()
    };
    for exec in [
        "codex",
        "gemini",
        "opencode",
        "cursor",
        "deepseek",
        "codebuddy",
        "kimi",
        "pi",
    ] {
        let cfg = RunConfig {
            executor: exec.to_string(),
            executor_cmd: Vec::new(),
            ..RunConfig::default()
        };
        assert!(
            build_runner_with(&cfg, env.clone()).is_ok(),
            "expected build_runner_with to succeed for {exec}"
        );
    }
}

#[test]
fn registry_headless_argv_emits_confined_and_readonly_flags() {
    let claude = find_cli("claude").unwrap();
    let codex = find_cli("codex").unwrap();
    let gemini = find_cli("gemini").unwrap();

    let claude_rw = build_headless_argv(claude, "/bin/claude", "fix", None, None, false).unwrap();
    assert!(has2(&claude_rw, "--permission-mode", "bypassPermissions"));
    let claude_unsandboxed =
        build_headless_argv_confined(claude, "/bin/claude", "fix", None, None, false, false)
            .unwrap();
    assert!(has2(
        &claude_unsandboxed,
        "--permission-mode",
        "acceptEdits"
    ));
    assert!(!claude_unsandboxed.contains(&"bypassPermissions".to_string()));

    let codex_rw = build_headless_argv(codex, "/bin/codex", "fix", None, None, false).unwrap();
    assert!(codex_rw.contains(&"--full-auto".to_string()));
    let codex_ro = build_headless_argv(codex, "/bin/codex", "audit", None, None, true).unwrap();
    assert!(has2(&codex_ro, "--sandbox", "read-only"));
    assert!(!codex_ro.contains(&"--full-auto".to_string()));

    let gemini_rw = build_headless_argv(gemini, "/bin/gemini", "fix", None, None, false).unwrap();
    assert!(has2(&gemini_rw, "--approval-mode", "yolo"));
    let gemini_ro = build_headless_argv(gemini, "/bin/gemini", "audit", None, None, true).unwrap();
    assert!(has2(&gemini_ro, "--approval-mode", "plan"));
}

#[test]
fn generic_runner_command_honors_sandbox_readonly_and_confined_flags() {
    let codex = GenericRunner::new(
        vec![
            "/bin/codex".into(),
            "exec".into(),
            "--".into(),
            "{prompt}".into(),
        ],
        ExecEnv::default(),
    )
    .unwrap();
    let mut ro_req = req("audit", "/tmp");
    ro_req.sandbox.enabled = true;
    ro_req.sandbox.read_only = true;
    let ro_spec = codex.command(&ro_req);
    assert_eq!(
        ro_spec.args,
        vec!["exec", "--sandbox", "read-only", "--", "audit"]
    );

    let mut rw_req = req("fix", "/tmp");
    rw_req.sandbox.enabled = true;
    let rw_spec = codex.command(&rw_req);
    assert_eq!(rw_spec.args, vec!["exec", "--full-auto", "--", "fix"]);

    let gemini = GenericRunner::new(
        vec!["/bin/gemini".into(), "-p".into(), "{prompt}".into()],
        ExecEnv::default(),
    )
    .unwrap();
    assert_eq!(
        gemini.command(&ro_req).args,
        vec!["--approval-mode", "plan", "-p", "audit"]
    );
    assert_eq!(
        gemini.command(&rw_req).args,
        vec!["--approval-mode", "yolo", "-p", "fix"]
    );
}

#[test]
fn generic_runner_classifies_quota_429_auth_401_and_5xx_network_failures() {
    let (lim, infra) = classify_generic_failure(
        Some(1),
        false,
        "Error: HTTP 429 Too Many Requests: rate_limit",
    );
    assert!(lim.is_some() && infra.is_none());

    let (lim, infra) =
        classify_generic_failure(Some(1), false, "HTTP 401 Unauthorized: invalid api key");
    assert!(lim.is_none());
    assert_eq!(infra.unwrap().kind, InfraKind::Auth);

    let (lim, infra) = classify_generic_failure(Some(1), false, "HTTP 503 Service Unavailable");
    assert!(lim.is_none());
    assert_eq!(infra.unwrap().kind, InfraKind::Network);

    // Success and timeout never trigger limit_hit/infra even if text mentions 429/401.
    assert_eq!(
        classify_generic_failure(Some(0), false, "HTTP 429"),
        (None, None)
    );
    assert_eq!(
        classify_generic_failure(None, true, "HTTP 401"),
        (None, None)
    );

    // End-to-end GenericRunner::run populates limit_hit & infra from child output.
    let dir = TempDir::new().unwrap();
    let cwd = dir.path().to_str().unwrap();
    let sh_runner = |script: &str| {
        GenericRunner::new(
            vec!["/bin/sh", "-c", script, "sh", "{prompt}"]
                .into_iter()
                .map(String::from)
                .collect(),
            ExecEnv::default(),
        )
        .unwrap()
    };
    let out_429 = sh_runner("echo 'HTTP 429 rate limit exceeded'; exit 1")
        .run(&req("p", cwd))
        .unwrap();
    assert!(out_429.limit_hit.is_some(), "{out_429:?}");
    assert!(out_429.infra.is_none());

    let out_401 = sh_runner("echo 'HTTP 401 Unauthorized'; exit 1")
        .run(&req("p", cwd))
        .unwrap();
    assert!(out_401.limit_hit.is_none());
    assert_eq!(
        out_401.infra.as_ref().map(|i| i.kind),
        Some(InfraKind::Auth)
    );
}

#[test]
fn generic_runner_account_paths_denies_sensitive_entries_with_and_without_account() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().to_string_lossy().into_owned();
    let env = ExecEnv {
        home: Some(home.clone()),
        ..ExecEnv::default()
    };
    let base_req = req("task", &home);

    // Default fallback when request.account == None.
    let assert_denied = |cmd: &[&str], label: &str, base: &str, rels: &[&str]| -> GenericRunner {
        let runner =
            GenericRunner::new(cmd.iter().map(|s| (*s).to_string()).collect(), env.clone())
                .unwrap();
        let paths = runner.account_paths(&base_req);
        for rel in rels {
            assert!(
                paths.write_denied.contains(&format!("{home}/{base}/{rel}")),
                "missing {label} write_denied {rel}: {:?}",
                paths.write_denied
            );
        }
        runner
    };
    let _ = assert_denied(
        &["/bin/gemini", "-p", "{prompt}"],
        "gemini",
        ".gemini",
        &[
            "GEMINI.md",
            "hooks",
            "settings.json",
            "settings.local.json",
            "extensions",
            "policies",
            "skills",
            "commands",
            "antigravity",
        ],
    );
    let _ = assert_denied(
        &["/bin/codex", "exec", "{prompt}"],
        "codex",
        ".codex",
        &[
            "config.toml",
            "hooks.json",
            "AGENTS.md",
            "instructions.md",
            "rules",
            "skills",
            "plugins",
        ],
    );
    let opencode = assert_denied(
        &["/bin/opencode", "run", "{prompt}"],
        "opencode",
        ".local/share/opencode",
        &[
            "opencode.json",
            "config.json",
            "plugin",
            "plugins",
            "agent",
            "agents",
            "command",
            "commands",
            "skill",
            "skills",
        ],
    );

    // Explicit OpenCode account also gets full write_denied.
    let mut acct_req = base_req;
    acct_req.account = Some(AccountProfile {
        id: "oc-1".into(),
        provider: ProviderKind::OpenCode,
        config_dir: format!("{home}/custom-oc"),
        name: "oc".into(),
    });
    let explicit_oc = opencode.account_paths(&acct_req);
    assert!(
        explicit_oc
            .write_denied
            .contains(&format!("{home}/custom-oc/opencode.json"))
    );
}

#[test]
fn verdict_parses_ansi_colored_gemini_pty_json_cleanly() {
    let spec = ImmutableSpec {
        description: "Verify ANSI stripping".into(),
        acceptance: vec!["Parses cleanly".into()],
    };
    let checklist = build_checklist(&spec, false);
    let answers: Vec<serde_json::Value> = checklist
        .iter()
        .map(|item| serde_json::json!({ "id": item.id, "answer": "no", "evidence": "" }))
        .collect();
    let json_text = serde_json::json!({ "answers": answers }).to_string();
    let ansi_wrapped = format!(
        "\x1b]0;gemini-cli\x07\x1b[?25l\x1b[32m{{\"answers\":\x1b[0m\x1b[36m{}\x1b[0m\x1b]8;;https://example.com\x1b\\done\x1b]8;;\x1b\\",
        json_text.strip_prefix("{\"answers\":").unwrap()
    );
    assert!(matches!(
        parse_verdict(&ansi_wrapped, &checklist),
        AuditVerdict::Pass { .. }
    ));
}

#[test]
fn bwrap_keeps_cache_roots_ro_binds_only_safe_subdirs_and_never_mounts_dev_null_on_missing_json() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let wt = root.join("worktree");
    let gradle = root.join("home/.gradle");
    let cargo = root.join("home/.cargo");
    let m2 = root.join("home/.m2");
    let pub_cache = root.join("home/.pub-cache");
    for d in [
        wt.clone(),
        gradle.join("caches"),
        gradle.join("wrapper"),
        cargo.join("registry"),
        m2.join("repository"),
        pub_cache.join("hosted"),
    ] {
        fs::create_dir_all(d).unwrap();
    }
    let missing_json = root.join("home/.gemini/settings.local.json");
    let paths = SandboxPaths {
        writable: vec![
            gradle.to_string_lossy().into_owned(),
            cargo.to_string_lossy().into_owned(),
            m2.to_string_lossy().into_owned(),
            pub_cache.to_string_lossy().into_owned(),
        ],
        write_denied: vec![missing_json.to_string_lossy().into_owned()],
        worktree: Some(wt.to_string_lossy().into_owned()),
        ..SandboxPaths::default()
    };
    let argv = build_bwrap_args(&paths, "/bin/true", &[]).unwrap();
    let s = |p: &Path| p.to_string_lossy().into_owned();

    // Roots MUST NOT be bound RW.
    for cache_root in [&gradle, &cargo, &m2, &pub_cache] {
        assert!(
            !has3(&argv, "--bind", &s(cache_root), &s(cache_root)),
            "root bound RW: {argv:?}"
        );
    }
    // Existing safe data subdirs ARE bound RW.
    for safe in [
        gradle.join("caches"),
        gradle.join("wrapper"),
        cargo.join("registry"),
        m2.join("repository"),
        pub_cache.join("hosted"),
    ] {
        assert!(
            has3(&argv, "--bind", &s(&safe), &s(&safe)),
            "safe subdir missing: {argv:?}"
        );
    }
    // Nonexistent subdirs (e.g. `.gradle/daemon`, `.cargo/git`) are NOT created and NOT bound.
    assert!(!gradle.join("daemon").exists());
    assert!(!cargo.join("git").exists());
    assert!(!has3(
        &argv,
        "--bind",
        &s(&gradle.join("daemon")),
        &s(&gradle.join("daemon"))
    ));
    // Missing `.json` never receives `/dev/null` bind and is never created on host.
    assert!(!missing_json.exists());
    assert!(!has3(&argv, "--ro-bind", "/dev/null", &s(&missing_json)));
}

#[test]
fn writable_caches_validation_rejects_dangerous_paths_and_accepts_safe_cache() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let home = root.join("alice");
    let data = root.join("aaf-data");
    let bin_dir = root.join("bin");
    let safe_cache = home.join(".cache/bazel");
    for d in [&home, &data, &bin_dir, &safe_cache] {
        fs::create_dir_all(d).unwrap();
    }
    let env = ExecEnv {
        home: Some(home.to_string_lossy().into_owned()),
        data_dir: Some(data.to_string_lossy().into_owned()),
        parent_env: vec![("PATH".into(), bin_dir.to_string_lossy().into_owned())],
        ..ExecEnv::default()
    };

    for bad in [
        "/".to_string(),
        home.to_string_lossy().into_owned(),
        root.to_string_lossy().into_owned(),
        "~".to_string(),
        "~/.ssh".to_string(),
        "~/.gnupg".to_string(),
        "~/.aws".to_string(),
        "~/.kube".to_string(),
        "~/.config/autofactory".to_string(),
        "~/.claude".to_string(),
        "~/.claude-work".to_string(),
        "~/.codex".to_string(),
        "~/.gemini".to_string(),
        "~/.local/share/opencode".to_string(),
        "~/.cache".to_string(),
        data.to_string_lossy().into_owned(),
        bin_dir.to_string_lossy().into_owned(),
        format!("{}/../bob", home.display()),
    ] {
        assert!(
            validate_writable_caches(std::slice::from_ref(&bad), &env).is_err(),
            "expected {bad:?} to be rejected"
        );
    }

    let ok = validate_writable_caches(&["~/.cache/bazel".to_string()], &env).unwrap();
    assert_eq!(ok, vec![safe_cache.to_string_lossy().into_owned()]);
}

#[test]
fn claude_unsandboxed_bypass_requires_explicit_opt_in() {
    let runner = ClaudeCodeRunner::new("/opt/bin/claude", ExecEnv::default());
    let default_args = runner.command(&req("task", "/tmp")).unwrap().args;
    assert!(has2(&default_args, "--permission-mode", "acceptEdits"));
    assert!(has2(&default_args, "--permission-prompts", "none"));

    let opted_in = runner.with_allow_unsandboxed_bypass(true);
    let bypass_args = opted_in.command(&req("task", "/tmp")).unwrap().args;
    assert!(has2(&bypass_args, "--permission-mode", "bypassPermissions"));

    let mut ro = req("audit", "/tmp");
    ro.sandbox.read_only = true;
    let ro_args = opted_in.command(&ro).unwrap().args;
    assert!(has2(&ro_args, "--permission-mode", "plan"));
}

#[test]
fn auto_audit_executor_and_readonly_audit_commands_and_pty_env() {
    let tmp = TempDir::new().unwrap();
    let bin = tmp.path().join("bin");
    let home = tmp.path().join("home");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(home.join(".codex")).unwrap();
    fake_bin(&bin, "codex");
    fake_bin(&bin, "gemini"); // no ~/.gemini dir -> skipped!
    let path_var = OsString::from(bin.as_os_str());

    let resolve = |exec: &str, h: &Path| resolve_auto_audit_executor("claude", exec, &path_var, h);
    assert_eq!(resolve("auto", &home), "codex");
    fs::create_dir_all(home.join(".gemini")).unwrap();
    assert_eq!(resolve("auto", &home), "gemini");
    // Falls back to run_executor when no other CLI is ready.
    let empty_home = tmp.path().join("empty-home");
    fs::create_dir_all(&empty_home).unwrap();
    assert_eq!(resolve("auto", &empty_home), "claude");
    // Explicit executor is preserved.
    assert_eq!(resolve("opencode", &home), "opencode");

    let audit_cmd = |exec: &str| {
        build_audit_command(&CrossAuditConfig {
            executor: exec.into(),
            ..Default::default()
        })
        .unwrap()
    };
    assert!(has2(&audit_cmd("codex"), "--sandbox", "read-only"));
    assert!(has2(&audit_cmd("gemini"), "--approval-mode", "plan"));

    // Toolchain allowlist + secret exclusion + modern build caches.
    let allowed = [
        ("GOPATH", "/go"),
        ("GOMODCACHE", "/go/pkg/mod"),
        ("PNPM_HOME", "/pnpm"),
        ("BUN_INSTALL", "/bun"),
        ("PUB_CACHE", "/pub"),
        ("FLUTTER_ROOT", "/flutter"),
        ("XDG_CACHE_HOME", "/cache"),
    ];
    let env = ExecEnv {
        parent_env: allowed
            .iter()
            .copied()
            .chain([
                ("OPENAI_API_KEY", "sk-secret"),
                ("GEMINI_API_KEY", "gm-secret"),
            ])
            .map(|(k, v)| (k.into(), v.into()))
            .collect(),
        ..ExecEnv::default()
    };
    let handed = agent_env(&env, &[]);
    for (key, _) in allowed {
        assert!(handed.iter().any(|(k, _)| k == key), "missing {key}");
    }
    assert!(
        !handed
            .iter()
            .any(|(k, _)| k == "OPENAI_API_KEY" || k == "GEMINI_API_KEY")
    );
    for cache_needle in [
        "pnpm",
        "bun",
        "deno",
        "ms-playwright",
        "ruff",
        "mypy",
        "pytest",
        "flutter",
    ] {
        assert!(
            BUILD_CACHE_DIRS.iter().any(|d| d.contains(cache_needle)),
            "missing {cache_needle}"
        );
    }

    // Default PTY non-interactive env injection.
    let work = TempDir::new().unwrap();
    let runner = GenericRunner::new(
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "/usr/bin/env; echo {prompt}".into(),
        ],
        ExecEnv::default(),
    )
    .unwrap();
    let out = runner
        .run(&req("ok", work.path().to_str().unwrap()))
        .unwrap();
    for expected in [
        "CI=true",
        "NONINTERACTIVE=1",
        "GIT_TERMINAL_PROMPT=0",
        "DEBIAN_FRONTEND=noninteractive",
        "NO_UPDATE_NOTIFIER=1",
    ] {
        assert!(
            out.log_tail.contains(expected),
            "missing {expected} in {}",
            out.log_tail
        );
    }
}
