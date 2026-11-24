#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests from audit round 4 (lane/audit4-fix).

use std::ffi::OsStr;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

use aaf_engine::hook_settings::{check_base, check_local, check_worktree, covering_hook};
use serde_json::json;

#[path = "../../aaf-fp/tests/exec_support.rs"]
mod exec_support;
use exec_support::write_executable;

#[test]
fn hook_entry_requires_command_type_and_positive_integer_timeout() {
    let missing_type = json!({
        "hooks": {"PreToolUse": [{
            "matcher": "Bash|Write|Edit|MultiEdit|Read",
            "hooks": [{"command": "aaf-hook"}],
        }]}
    });
    assert!(
        covering_hook(&missing_type.to_string()).is_err(),
        "entry without type: \"command\" must be rejected"
    );

    for bad_timeout in [
        json!(0),
        json!(-5),
        json!(0.5),
        json!("30"),
        json!(null),
        json!(true),
    ] {
        let cfg = json!({
            "hooks": {"PreToolUse": [{
                "matcher": "Bash|Write|Edit|MultiEdit|Read",
                "hooks": [{"type": "command", "command": "aaf-hook", "timeout": bad_timeout}],
            }]}
        });
        assert!(
            covering_hook(&cfg.to_string()).is_err(),
            "timeout={bad_timeout} must be rejected"
        );
    }

    let ok = json!({
        "hooks": {"PreToolUse": [{
            "matcher": "Bash|Write|Edit|MultiEdit|Read",
            "hooks": [{"type": "command", "command": "aaf-hook", "timeout": 30}],
        }]}
    });
    assert!(covering_hook(&ok.to_string()).is_ok());
}

#[test]
fn settings_env_overrides_and_committed_local_settings_on_base_are_rejected() {
    for var in [
        "AAF_DATA_DIR",
        "AAF_CONFIG_DIR",
        "AAF_WORKTREE_DIR",
        "AAF_RUN_ID",
        "AAF_HEAVY_SLOT",
        "HOME",
        "XDG_CONFIG_HOME",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
        "GIT_DIR",
        "GIT_COMMON_DIR",
        "DYLD_INSERT_LIBRARIES",
        "LD_PRELOAD",
    ] {
        let settings = json!({
            "env": { var: "/tmp/attacker" },
            "hooks": {"PreToolUse": [{
                "matcher": "Bash|Write|Edit|MultiEdit|Read",
                "hooks": [{"type": "command", "command": "/usr/local/bin/aaf-hook"}],
            }]}
        });
        assert!(
            covering_hook(&settings.to_string()).is_err(),
            "settings.json env.{var} must be rejected"
        );
        let local = json!({ "env": { var: "/tmp/attacker" } });
        assert!(
            check_local(&local.to_string()).is_err(),
            "settings.local.json env.{var} must be rejected"
        );
    }

    // Ordinary env vars stay allowed in both files.
    let benign_settings = json!({
        "env": { "RUST_LOG": "info" },
        "hooks": {"PreToolUse": [{
            "matcher": "Bash|Write|Edit|MultiEdit|Read",
            "hooks": [{"type": "command", "command": "aaf-hook"}],
        }]}
    });
    assert!(covering_hook(&benign_settings.to_string()).is_ok());
    assert!(check_local(&json!({ "env": { "RUST_LOG": "info" } }).to_string()).is_ok());

    // Committed `.claude/settings.local.json` on base disabling hooks is caught by `check_base`.
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let bin = tmp.path().join("bin");
    fs::create_dir_all(repo.join(".claude")).unwrap();
    fs::create_dir_all(&bin).unwrap();
    let hook = bin.join("aaf-hook");
    write_executable(&hook, "#!/bin/sh\necho '{}'\n");
    let git = |args: &[&str]| {
        let ok = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    fs::write(
        repo.join(".claude/settings.json"),
        json!({
            "hooks": {"PreToolUse": [{
                "matcher": "Bash|Write|Edit|MultiEdit|Read",
                "hooks": [{"type": "command", "command": "aaf-hook"}],
            }]}
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        repo.join(".claude/settings.local.json"),
        json!({"disableAllHooks": true}).to_string(),
    )
    .unwrap();
    git(&[
        "add",
        "-f",
        ".claude/settings.json",
        ".claude/settings.local.json",
    ]);
    git(&["commit", "-q", "-m", "commit local override"]);
    let err = check_base(&repo, "main", OsStr::new(&bin)).unwrap_err();
    assert!(err.contains("settings.local.json"), "{err}");
}

#[cfg(target_os = "macos")]
#[test]
fn symlinked_write_denied_targets_inside_writable_caches_stay_closed() {
    use std::os::unix::fs::symlink;
    use std::path::Path;

    use aaf_domain::ports::{AgentRequest, AgentRunner, SandboxPolicy};
    use aaf_engine::executors::ExecEnv;
    use aaf_engine::executors::claude::ClaudeCodeRunner;

    if !Path::new("/usr/bin/sandbox-exec").exists() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let home = root.join("home");
    let gradle = home.join(".gradle");
    let cargo = home.join(".cargo");
    let wt = root.join("wt");
    let my_init = gradle.join("my-init");
    fs::create_dir_all(&my_init).unwrap();
    fs::create_dir_all(gradle.join("caches")).unwrap();
    fs::create_dir_all(&cargo).unwrap();
    fs::create_dir_all(&wt).unwrap();
    symlink(&my_init, gradle.join("init.d")).unwrap();

    let my_cfg = cargo.join("my-config");
    let my_cfg_toml = cargo.join("my-config.toml");
    fs::write(&my_cfg, "kept\n").unwrap();
    fs::write(&my_cfg_toml, "kept\n").unwrap();
    symlink(&my_cfg, cargo.join("config")).unwrap();
    symlink(&my_cfg_toml, cargo.join("config.toml")).unwrap();

    let probe = |marker: &str, p: &Path| {
        format!(
            "( echo evil > '{}' ) 2>/dev/null && echo {marker}-ok || echo {marker}-denied\n",
            p.display()
        )
    };
    let script = format!(
        "#!/bin/sh\n{}{}{}{}{}{}",
        probe("via-link", &gradle.join("init.d/evil.gradle")),
        probe("via-target", &my_init.join("evil.gradle")),
        probe("prefix-link", &cargo.join("config")),
        probe("prefix-target", &my_cfg),
        probe("prefix-toml-target", &my_cfg_toml),
        probe("cache", &gradle.join("caches/ok")),
    );
    let claude = root.join("claude");
    write_executable(&claude, script);

    let env = ExecEnv {
        home: Some(home.to_string_lossy().into_owned()),
        temp_dirs: Vec::new(),
        parent_env: vec![("PATH".to_string(), "/usr/bin:/bin".to_string())],
        ..ExecEnv::default()
    };
    let log = ClaudeCodeRunner::new(claude.to_string_lossy(), env)
        .run(&AgentRequest {
            run_id: format!("test-{}", uuid::Uuid::new_v4()),
            prompt: "unused".to_string(),
            cwd: wt.to_string_lossy().into_owned(),
            account: None,
            timeout_secs: 60,
            sandbox: SandboxPolicy {
                enabled: true,
                ..SandboxPolicy::default()
            },
            json_schema: None,
        })
        .unwrap()
        .log_tail;

    for marker in [
        "via-link-denied",
        "via-target-denied",
        "prefix-link-denied",
        "prefix-target-denied",
        "prefix-toml-target-denied",
        "cache-ok",
    ] {
        assert!(log.contains(marker), "missing {marker}: {log}");
    }
    assert!(!my_init.join("evil.gradle").exists());
    assert_eq!(fs::read_to_string(&my_cfg).unwrap(), "kept\n");
    assert_eq!(fs::read_to_string(&my_cfg_toml).unwrap(), "kept\n");
}

#[test]
fn env_pass_refuses_additional_git_and_shell_injection_vars() {
    use aaf_domain::config::RunConfig;
    use aaf_domain::ports::PortError;
    use aaf_engine::executors::{ExecEnv, build_runner_with};

    let make_cfg = |var: &str| RunConfig {
        executor: "custom".to_string(),
        executor_cmd: vec!["/bin/echo".to_string(), "{prompt}".to_string()],
        env_pass: vec![var.to_string()],
        ..RunConfig::default()
    };
    for var in [
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_TEMPLATE_DIR",
        "GIT_EXTERNAL_DIFF",
        "GIT_PROXY_COMMAND",
        "GIT_ASKPASS",
        "SSH_ASKPASS",
        "BASH_ENV",
        "ENV",
        "ZDOTDIR",
        "NODE_OPTIONS",
        "PYTHONSTARTUP",
        "node_options",
    ] {
        let err = build_runner_with(&make_cfg(var), ExecEnv::default())
            .err()
            .unwrap_or_else(|| panic!("{var:?} was accepted"));
        assert!(matches!(err, PortError::Invalid(_)), "{var:?}: {err:?}");
    }
    for ok in ["SSH_AUTH_SOCK", "GIT_AUTHOR_NAME", "APP_ENV", "ENV_NAME"] {
        assert!(
            build_runner_with(&make_cfg(ok), ExecEnv::default()).is_ok(),
            "{ok:?} was falsely rejected"
        );
    }
}

#[test]
fn throttle_heavy_classifier_handles_clustered_env_p_s_and_pty_wrappers() {
    use aaf_engine::throttle::heavy::{HeavyTool, heavy_tool, heavy_tool_argv};

    for argv in [
        vec!["env", "-P", "/tmp/bin", "./gradlew", "test"],
        vec!["env", "-P/tmp/bin", "./gradlew", "test"],
        vec!["env", "-iP", "/tmp/bin", "./gradlew", "test"],
        vec!["env", "-iu", "FOO", "./gradlew", "test"],
        vec!["env", "-S", "./gradlew test"],
        vec!["env", "-iS", "./gradlew test"],
        vec!["env", "--split-string=./gradlew test"],
        vec!["sudo", "-Eu", "root", "./gradlew", "test"],
        vec!["xargs", "-0n", "1", "./gradlew", "test"],
        vec!["unbuffer", "./gradlew", "test"],
        vec!["script", "-q", "/dev/null", "./gradlew", "test"],
        vec!["script", "-c", "./gradlew test", "/dev/null"],
        vec!["script", "-c./gradlew test", "/dev/null"],
        vec!["script", "--command=./gradlew test", "/dev/null"],
    ] {
        assert_eq!(
            heavy_tool_argv(&argv),
            Some(HeavyTool::Gradle),
            "heavy_tool_argv missed {argv:?}"
        );
    }
    assert_eq!(
        heavy_tool("env -iu FOO ./gradlew test"),
        Some(HeavyTool::Gradle)
    );
    assert_eq!(
        heavy_tool("script -q /dev/null ./gradlew test"),
        Some(HeavyTool::Gradle)
    );
    // Plain non-heavy command through wrappers stays None.
    assert_eq!(
        heavy_tool_argv(&["env", "-iu", "FOO", "echo", "gradle"]),
        None
    );
    assert_eq!(
        heavy_tool_argv(&["script", "-q", "/dev/null", "echo", "gradle"]),
        None
    );
}

#[test]
fn check_local_allows_non_pretooluse_hooks_and_rejects_pretooluse_and_non_object_hooks() {
    for ok in [
        json!({}),
        json!({"hooks": {}}),
        json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "say done"}]}]}}),
        json!({"hooks": {"PostToolUse": []}}),
        json!({"disableAllHooks": false}),
    ] {
        assert!(check_local(&ok.to_string()).is_ok(), "expected Ok for {ok}");
    }

    for bad in [
        json!({"hooks": {"PreToolUse": []}}),
        json!({"hooks": {"pretooluse": []}}),
        json!({"hooks": []}),
        json!({"hooks": null}),
        json!({"hooks": true}),
        json!({"hooks": "nope"}),
        json!({"disableAllHooks": true}),
        json!({"disableAllHooks": "false"}),
        json!({"disableAllHooks": null}),
    ] {
        assert!(
            check_local(&bad.to_string()).is_err(),
            "expected Err for {bad}"
        );
    }
}

#[test]
fn check_worktree_rejects_local_settings_created_changed_or_deleted_relative_to_merge_base() {
    let setup = |base_local: Option<&str>| {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let bin = tmp.path().join("bin");
        fs::create_dir_all(repo.join(".claude")).unwrap();
        fs::create_dir_all(&bin).unwrap();
        let hook = bin.join("aaf-hook");
        fs::write(&hook, "#!/bin/sh\necho '{}'\n").unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        fs::write(
            repo.join(".claude/settings.json"),
            json!({
                "hooks": {"PreToolUse": [{
                    "matcher": "Bash|Write|Edit|MultiEdit|Read",
                    "hooks": [{"type": "command", "command": "aaf-hook"}],
                }]}
            })
            .to_string(),
        )
        .unwrap();
        git(&["add", "-f", ".claude/settings.json"]);
        if let Some(local) = base_local {
            fs::write(repo.join(".claude/settings.local.json"), local).unwrap();
            git(&["add", "-f", ".claude/settings.local.json"]);
        }
        git(&["commit", "-q", "-m", "base"]);
        git(&["checkout", "-q", "-b", "aaf/task-1"]);
        (tmp, repo, bin)
    };

    // Case 1: absent at merge-base -> must remain absent in the task worktree.
    let (_tmp, repo, bin) = setup(None);
    check_worktree(&repo, OsStr::new(&bin), Some("main")).unwrap();
    fs::write(
        repo.join(".claude/settings.local.json"),
        json!({"permissions": {"allow": ["Bash(ls:*)"]}}).to_string(),
    )
    .unwrap();
    let err = check_worktree(&repo, OsStr::new(&bin), Some("main")).unwrap_err();
    assert!(err.contains("settings.local.json"), "{err}");
    assert!(err.contains("not on the base branch"), "{err}");

    // Case 2: present at merge-base -> unchanged passes; modified or deleted fails.
    let (_tmp, repo, bin) = setup(Some("{\"permissions\": {}}\n"));
    check_worktree(&repo, OsStr::new(&bin), Some("main")).unwrap();

    fs::write(
        repo.join(".claude/settings.local.json"),
        "{\"permissions\": {\"allow\": [\"Bash(*)\"]}}\n",
    )
    .unwrap();
    let err = check_worktree(&repo, OsStr::new(&bin), Some("main")).unwrap_err();
    assert!(err.contains("settings.local.json"), "{err}");
    assert!(err.contains("not what the base branch has"), "{err}");

    fs::remove_file(repo.join(".claude/settings.local.json")).unwrap();
    let err = check_worktree(&repo, OsStr::new(&bin), Some("main")).unwrap_err();
    assert!(err.contains("settings.local.json"), "{err}");
    assert!(err.contains("deleted"), "{err}");
}
