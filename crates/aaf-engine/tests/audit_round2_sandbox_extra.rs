#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Companion regression tests for Round 2A Fixer B: Linux Bubblewrap missing-file
//! overlays & symlink targets, tightened `run.env_pass` credential rules, custom
//! toolchain homes + Codex config guards, live `GenericRunner` stream logging,
//! and strict `hook_settings` PATH / matcher validation.

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::symlink;

use aaf_domain::config::RunConfig;
#[cfg(unix)]
use aaf_domain::ports::{AgentRequest, AgentRunner, SandboxPolicy};
#[cfg(unix)]
use aaf_engine::executors::generic::GenericRunner;
use aaf_engine::executors::linux_sandbox::build_bwrap_args;
use aaf_engine::executors::sandbox::SandboxPaths;
#[cfg(unix)]
use aaf_engine::executors::stream_log::read_tail_lines;
use aaf_engine::executors::{ExecEnv, build_runner_with};
use aaf_engine::hook_settings::multi_host::{
    codex_matcher, covering_codex_hook, covering_gemini_hook, gemini_matcher,
};
use aaf_engine::hook_settings::{HookChange, covering_hook, find_hooks, merge_hook};
use serde_json::json;
use tempfile::TempDir;

#[test]
fn bwrap_pins_ancestors_skips_missing_paths_and_protects_worktrees_root_order() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let wt_root = root.join("worktrees");
    let wt = wt_root.join("repo/task-1");
    let git_dir = root.join("repo.git");
    let wt_meta = git_dir.join("worktrees/task-1");
    let cargo = root.join("home/.cargo");
    let gradle = root.join("home/.gradle");
    let gradle_real_init = gradle.join("real-init");
    for d in [
        wt.join(".claude"),
        git_dir.join("refs/aaf"),
        wt_meta.join("hooks"),
        cargo.clone(),
        gradle_real_init.clone(),
    ] {
        fs::create_dir_all(d).unwrap();
    }
    fs::write(wt.join(".claude/settings.json"), "{}").unwrap();
    fs::write(git_dir.join("config.worktree"), "[core]\n").unwrap();
    fs::write(wt_meta.join("config"), "[core]\n").unwrap();
    fs::write(wt_meta.join("commondir"), "../..\n").unwrap();
    #[cfg(unix)]
    {
        symlink(&gradle_real_init, gradle.join("init.d")).unwrap();
        symlink(cargo.join("missing-target"), cargo.join("dangling")).unwrap();
    }

    let paths = SandboxPaths {
        writable: vec![
            cargo.to_string_lossy().into_owned(),
            gradle.to_string_lossy().into_owned(),
        ],
        write_denied: vec![
            cargo.join("env").to_string_lossy().into_owned(),
            cargo.join("dangling").to_string_lossy().into_owned(),
            gradle.join("init.d").to_string_lossy().into_owned(),
        ],
        write_denied_prefix: vec![
            cargo.join("config").to_string_lossy().into_owned(),
            gradle.join("init.gradle").to_string_lossy().into_owned(),
        ],
        worktree: Some(wt.to_string_lossy().into_owned()),
        worktrees_root: Some(wt_root.to_string_lossy().into_owned()),
        git_dirs: vec![git_dir.to_string_lossy().into_owned()],
        ..SandboxPaths::default()
    };
    let argv = build_bwrap_args(&paths, "/bin/true", &[]).unwrap();
    let has3 =
        |a: &str, b: &str, c: &str| argv.windows(3).any(|w| w[0] == a && w[1] == b && w[2] == c);
    let pos3 = |a: &str, b: &str, c: &str| {
        argv.windows(3)
            .position(|w| w[0] == a && w[1] == b && w[2] == c)
    };

    // `worktrees_root` is protected RO before active `worktree` is bound RW.
    let root_ro = pos3(
        "--ro-bind",
        wt_root.to_str().unwrap(),
        wt_root.to_str().unwrap(),
    )
    .unwrap();
    let wt_rw = pos3("--bind", wt.to_str().unwrap(), wt.to_str().unwrap()).unwrap();
    assert!(root_ro < wt_rw, "{argv:?}");

    // Intermediate parent mountpoints are pinned via `--bind <dir> <dir>` BEFORE
    // their RO-bound children so `mv` on parents fails with EBUSY.
    let claude_dir = wt.join(".claude");
    let claude_settings = wt.join(".claude/settings.json");
    let claude_pin = pos3(
        "--bind",
        claude_dir.to_str().unwrap(),
        claude_dir.to_str().unwrap(),
    )
    .unwrap();
    let settings_ro = pos3(
        "--ro-bind",
        claude_settings.to_str().unwrap(),
        claude_settings.to_str().unwrap(),
    )
    .unwrap();
    assert!(claude_pin < settings_ro, "{argv:?}");

    let refs_dir = git_dir.join("refs");
    let refs_aaf = git_dir.join("refs/aaf");
    let refs_pin = pos3(
        "--bind",
        refs_dir.to_str().unwrap(),
        refs_dir.to_str().unwrap(),
    )
    .unwrap();
    let refs_aaf_ro = pos3(
        "--ro-bind",
        refs_aaf.to_str().unwrap(),
        refs_aaf.to_str().unwrap(),
    )
    .unwrap();
    assert!(refs_pin < refs_aaf_ro, "{argv:?}");

    // `<git_dir>/worktrees` is bound RO, then existing `<child>` is bound RW,
    // then existing sensitive entries under `<child>` are bound RO.
    let git_wts = git_dir.join("worktrees");
    let git_wts_ro = pos3(
        "--ro-bind",
        git_wts.to_str().unwrap(),
        git_wts.to_str().unwrap(),
    )
    .unwrap();
    assert!(
        !has3(
            "--bind",
            git_wts.to_str().unwrap(),
            git_wts.to_str().unwrap()
        ),
        "worktrees dir must stay read-only: {argv:?}"
    );
    let wt_meta_rw = pos3(
        "--bind",
        wt_meta.to_str().unwrap(),
        wt_meta.to_str().unwrap(),
    )
    .unwrap();
    let wt_meta_cfg_ro = pos3(
        "--ro-bind",
        wt_meta.join("config").to_str().unwrap(),
        wt_meta.join("config").to_str().unwrap(),
    )
    .unwrap();
    assert!(
        git_wts_ro < wt_meta_rw && wt_meta_rw < wt_meta_cfg_ro,
        "{argv:?}"
    );

    // Existing `config.worktree` is RO-bound.
    assert!(has3(
        "--ro-bind",
        git_dir.join("config.worktree").to_str().unwrap(),
        git_dir.join("config.worktree").to_str().unwrap(),
    ));

    // Non-existent paths and dangling symlinks MUST NOT emit `--ro-bind /dev/null`
    // or host-polluting `--tmpfs` overlays inside RW mounts.
    for missing_file in [
        wt.join(".claude/settings.local.json"),
        wt_meta.join("gitdir"),
        cargo.join("env"),
        cargo.join("dangling"),
        cargo.join("missing-target"),
        cargo.join("config"),
        cargo.join("config.toml"),
        gradle.join("init.gradle"),
        gradle.join("init.gradle.kts"),
    ] {
        let s = missing_file.to_str().unwrap();
        assert!(
            !has3("--ro-bind", "/dev/null", s),
            "must not emit --ro-bind /dev/null for missing {}: {argv:?}",
            missing_file.display()
        );
        assert!(
            !argv.windows(2).any(|w| w[0] == "--tmpfs" && w[1] == s),
            "must not emit --tmpfs for missing {}: {argv:?}",
            missing_file.display()
        );
    }

    #[cfg(unix)]
    assert!(
        has3(
            "--ro-bind",
            gradle_real_init.to_str().unwrap(),
            gradle_real_init.to_str().unwrap()
        ),
        "symlink target for existing write_denied must also be ro-bound: {argv:?}"
    );
}

#[test]
fn bwrap_pin_ancestors_respects_ro_trees_canonical_dedup_and_skips_missing_read_denied() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let wt_root = root.join("worktrees");
    let wt = wt_root.join("repo/task-1");
    let sibling_tool = wt_root.join("repo/task-2/sub/tool");
    let git_dir = root.join("repo.git");
    let hook_sub = git_dir.join("hooks/sub");
    let wt_meta_nested = git_dir.join("worktrees/task-1/nested");
    let real_ro = wt.join("real-ro");
    let real_sub = wt.join("real-sub");
    #[cfg(unix)]
    let (link_ro, link_sub) = (wt.join("link-ro"), wt.join("link-sub"));
    let data_dir = root.join("aaf-data");
    let secret_file = root.join("secret.key");
    let missing_denied = root.join("does-not-exist");

    for d in [
        wt.clone(),
        sibling_tool.parent().unwrap().to_path_buf(),
        hook_sub.clone(),
        wt_meta_nested.clone(),
        real_ro.clone(),
        real_sub.clone(),
        data_dir.clone(),
    ] {
        fs::create_dir_all(d).unwrap();
    }
    fs::write(hook_sub.join("hook.sh"), "#!/bin/sh\n").unwrap();
    fs::write(wt_meta_nested.join("secret"), "x\n").unwrap();
    fs::write(&sibling_tool, "#!/bin/sh\n").unwrap();
    fs::write(real_ro.join("file.txt"), "ro\n").unwrap();
    fs::write(real_sub.join("a.txt"), "a\n").unwrap();
    fs::write(real_sub.join("b.txt"), "b\n").unwrap();
    fs::write(&secret_file, "top-secret\n").unwrap();
    #[cfg(unix)]
    {
        symlink(&real_ro, &link_ro).unwrap();
        symlink(&real_sub, &link_sub).unwrap();
    }

    let mut write_denied = vec![
        hook_sub.join("hook.sh").to_string_lossy().into_owned(),
        wt_meta_nested.join("secret").to_string_lossy().into_owned(),
        sibling_tool.to_string_lossy().into_owned(),
        real_ro.to_string_lossy().into_owned(),
        real_sub.join("a.txt").to_string_lossy().into_owned(),
    ];
    #[cfg(unix)]
    {
        write_denied.push(link_ro.join("file.txt").to_string_lossy().into_owned());
        write_denied.push(link_sub.join("b.txt").to_string_lossy().into_owned());
    }
    let paths = SandboxPaths {
        writable: vec![root.to_string_lossy().into_owned()],
        write_denied,
        read_denied: vec![
            data_dir.to_string_lossy().into_owned(),
            secret_file.to_string_lossy().into_owned(),
            missing_denied.to_string_lossy().into_owned(),
        ],
        worktree: Some(wt.to_string_lossy().into_owned()),
        worktrees_root: Some(wt_root.to_string_lossy().into_owned()),
        git_dirs: vec![git_dir.to_string_lossy().into_owned()],
        ..SandboxPaths::default()
    };
    let argv = build_bwrap_args(&paths, "/bin/true", &[]).unwrap();
    let has3 =
        |a: &str, b: &str, c: &str| argv.windows(3).any(|w| w[0] == a && w[1] == b && w[2] == c);
    let has2 = |a: &str, b: &str| argv.windows(2).any(|w| w[0] == a && w[1] == b);

    // Finding 1: never punch an RW `--bind` hole inside RO `hooks` or RO `worktrees_root`.
    for forbidden_rw in [
        &hook_sub,
        &wt_root.join("repo/task-2"),
        &wt_root.join("repo/task-2/sub"),
        &git_dir.join("worktrees"),
    ] {
        let s = forbidden_rw.to_str().unwrap();
        assert!(
            !has3("--bind", s, s),
            "must not punch RW hole at {s}: {argv:?}"
        );
    }
    // `<git_dir>/worktrees/task-1` is registered in `bound_rw`, so its nested subdir IS pinned.
    let nested_s = wt_meta_nested.to_str().unwrap();
    assert!(has3("--bind", nested_s, nested_s), "{argv:?}");

    // Finding 2: canonical dedup prevents symlinked paths from mounting RW over RO or duplicating pins.
    #[cfg(unix)]
    {
        for p in [&link_ro, &real_ro, &link_sub] {
            let s = p.to_str().unwrap();
            assert!(!has3("--bind", s, s), "unexpected --bind on {s}: {argv:?}");
        }
        let real_sub_s = real_sub.to_str().unwrap();
        let pin_count = argv
            .windows(3)
            .filter(|w| w[0] == "--bind" && w[1] == real_sub_s && w[2] == real_sub_s)
            .count();
        assert_eq!(pin_count, 1, "canonical pin must be emitted once: {argv:?}");
    }

    // Finding 3: `read_denied` uses `--tmpfs` for existing dirs, `--ro-bind /dev/null` for
    // existing files, and skips non-existent paths entirely.
    assert!(has2("--tmpfs", data_dir.to_str().unwrap()), "{argv:?}");
    assert!(
        has3("--ro-bind", "/dev/null", secret_file.to_str().unwrap()),
        "{argv:?}"
    );
    let missing_s = missing_denied.to_str().unwrap();
    assert!(!has2("--tmpfs", missing_s), "{argv:?}");
    assert!(!has3("--ro-bind", "/dev/null", missing_s), "{argv:?}");
}

#[test]
fn env_pass_tightening_and_generic_stream_log_and_hook_settings_hardening() {
    let cfg = |vars: &[&str]| RunConfig {
        executor: "custom".to_string(),
        executor_cmd: vec!["/bin/echo".to_string(), "{prompt}".to_string()],
        env_pass: vars.iter().map(|s| (*s).to_string()).collect(),
        ..RunConfig::default()
    };
    for secret_var in [
        "TOKEN",
        "SECRET",
        "GITHUB_TOKEN_V2",
        "API_KEY_ID",
        "DB_PASSWD",
        "GOOGLE_APPLICATION_CREDENTIALS",
        "DATABASE_URL",
        "AUTH_HEADER",
        "BEARER_AUTH",
        "SESSION_COOKIE",
    ] {
        assert!(
            build_runner_with(&cfg(&[secret_var]), ExecEnv::default()).is_err(),
            "{secret_var} must be refused in tier-2 env_pass"
        );
        let machine = ExecEnv {
            machine_env_pass: vec![secret_var.to_string()],
            ..ExecEnv::default()
        };
        assert!(build_runner_with(&cfg(&[]), machine).is_ok());
    }
    for benign in ["PUBLIC_KEY", "SSH_PUBLIC_KEY", "KEYMAP", "SSH_AUTH_SOCK"] {
        assert!(build_runner_with(&cfg(&[benign]), ExecEnv::default()).is_ok());
    }

    // Hook settings: `"env"."PATH"` is always forbidden even for absolute paths;
    // non-string `"matcher"` values are rejected instead of collapsing to `""`.
    let abs_claude_path = json!({
        "env": {"PATH": "/tmp/evil"},
        "hooks": {"PreToolUse": [{
            "matcher": "Bash|Write|Edit|MultiEdit|Read",
            "hooks": [{"type": "command", "command": "/usr/local/bin/aaf-hook"}]
        }]}
    });
    assert!(covering_hook(&abs_claude_path.to_string()).is_err());
    let abs_codex_path = json!({
        "env": {"PATH": "/tmp/evil"},
        "hooks": {"PreToolUse": [{
            "matcher": codex_matcher(),
            "hooks": [{"type": "command", "command": "/usr/local/bin/aaf-hook --host codex"}]
        }]}
    });
    assert!(covering_codex_hook(&abs_codex_path.to_string()).is_err());
    let abs_gemini_path = json!({
        "env": {"PATH": "/tmp/evil"},
        "hooks": {"BeforeTool": [{
            "matcher": gemini_matcher(),
            "hooks": [{"type": "command", "command": "/usr/local/bin/aaf-hook --host gemini"}]
        }]}
    });
    assert!(covering_gemini_hook(&abs_gemini_path.to_string()).is_err());

    for bad_matcher in [json!(null), json!(123), json!(false), json!([]), json!({})] {
        let claude_bad = json!({
            "hooks": {"PreToolUse": [{
                "matcher": bad_matcher,
                "hooks": [{"type": "command", "command": "aaf-hook"}]
            }]}
        })
        .to_string();
        assert!(covering_hook(&claude_bad).is_err(), "{bad_matcher}");
        assert!(find_hooks(&claude_bad).unwrap().is_empty());
        let (healed, change) = merge_hook(Some(&claude_bad), "aaf-hook").unwrap();
        assert_eq!(change, HookChange::Updated);
        assert!(covering_hook(&healed.unwrap()).is_ok());

        let codex_bad = json!({
            "hooks": {"PreToolUse": [{
                "matcher": bad_matcher,
                "hooks": [{"type": "command", "command": "aaf-hook --host codex"}]
            }]}
        });
        assert!(covering_codex_hook(&codex_bad.to_string()).is_err());
        let gemini_bad = json!({
            "hooks": {"BeforeTool": [{
                "matcher": bad_matcher,
                "hooks": [{"type": "command", "command": "aaf-hook --host gemini"}]
            }]}
        });
        assert!(covering_gemini_hook(&gemini_bad.to_string()).is_err());
    }

    // GenericRunner streams live scrubbed JSONL events via `on_line`.
    #[cfg(unix)]
    {
        let dir = TempDir::new().unwrap();
        let data = dir.path().join("data");
        let work = dir.path().join("work");
        fs::create_dir_all(&work).unwrap();
        let tok = ["ghp_", "0123456789abcdef0123456789abcdef0123"].concat();
        let script = format!(
            "printf '%s\\n' '{{\"type\":\"item.started\",\"item\":{{\"type\":\"command_execution\",\"command\":\"curl {tok}\"}}}}'\n\
             printf '%s\\n' '{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":\"all checks green\"}}}}'\n"
        );
        let runner = GenericRunner::new(
            ["/bin/sh", "-c", &script, "codex", "{prompt}"]
                .map(String::from)
                .to_vec(),
            ExecEnv {
                data_dir: Some(data.to_string_lossy().into_owned()),
                ..ExecEnv::default()
            },
        )
        .unwrap();
        let out = runner
            .run(&AgentRequest {
                run_id: "run_generic_live_1".to_string(),
                prompt: "go".to_string(),
                cwd: work.to_string_lossy().into_owned(),
                account: None,
                timeout_secs: 30,
                sandbox: SandboxPolicy::default(),
                json_schema: None,
            })
            .unwrap();
        assert_eq!(out.exit_code, Some(0));
        let lines = read_tail_lines(&data, "run_generic_live_1", 10).unwrap();
        let joined = lines.join("\n");
        assert!(!joined.contains(&tok), "{joined}");
        assert!(joined.contains("\"tool_use\""), "{joined}");
        assert!(joined.contains("\"assistant\""), "{joined}");
        assert!(joined.contains("all checks green"), "{joined}");
    }
}

#[cfg(target_os = "macos")]
#[test]
fn custom_toolchain_homes_and_codex_config_stay_guarded_in_seatbelt() {
    use std::path::Path;

    use aaf_domain::account::{AccountProfile, ProviderKind};

    if !Path::new("/usr/bin/sandbox-exec").exists() {
        return;
    }
    let dir = TempDir::new().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let wt = root.join("wt");
    let cargo = root.join("custom-cargo");
    let rustup = root.join("custom-rustup");
    let gradle = root.join("custom-gradle");
    let codex = root.join("codex-acct");
    for d in [
        wt.clone(),
        cargo.join("registry"),
        cargo.join("bin"),
        rustup.join("toolchains"),
        gradle.join("caches"),
        gradle.join("init.d"),
        codex.join("sessions"),
    ] {
        fs::create_dir_all(d).unwrap();
    }
    let probe = |tag: &str, p: &Path| {
        format!(
            "( echo x > '{}' ) 2>/dev/null && echo {tag}-ok || echo {tag}-denied\n",
            p.display()
        )
    };
    let script = [
        probe("cargo-cache", &cargo.join("registry/ok")),
        probe("rustup-ok", &rustup.join("toolchains/ok")),
        probe("gradle-cache", &gradle.join("caches/ok")),
        probe("codex-state", &codex.join("sessions/ok")),
        probe("cargo-bin", &cargo.join("bin/evil")),
        probe("cargo-cfg", &cargo.join("config.toml")),
        probe("gradle-props", &gradle.join("gradle.properties")),
        probe("gradle-init", &gradle.join("init.gradle.kts")),
        probe("codex-cfg", &codex.join("config.toml")),
        probe("codex-hooks", &codex.join("hooks.json")),
    ]
    .concat();
    let env = ExecEnv {
        temp_dirs: Vec::new(),
        cargo_home: Some(cargo.to_string_lossy().into_owned()),
        rustup_home: Some(rustup.to_string_lossy().into_owned()),
        gradle_user_home: Some(gradle.to_string_lossy().into_owned()),
        parent_env: vec![("PATH".to_string(), "/usr/bin:/bin".to_string())],
        ..ExecEnv::default()
    };
    let runner = GenericRunner::new(
        ["/bin/sh", "-c", &script, "codex", "{prompt}"]
            .map(String::from)
            .to_vec(),
        env,
    )
    .unwrap();
    let log = runner
        .run(&AgentRequest {
            run_id: format!("test-custom-homes-{}", std::process::id()),
            prompt: "go".to_string(),
            cwd: wt.to_string_lossy().into_owned(),
            account: Some(AccountProfile {
                id: "codex-test".to_string(),
                provider: ProviderKind::Codex,
                config_dir: codex.to_string_lossy().into_owned(),
                name: "test".to_string(),
            }),
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
        "cargo-cache-ok",
        "rustup-ok-ok",
        "gradle-cache-ok",
        "codex-state-ok",
        "cargo-bin-denied",
        "cargo-cfg-denied",
        "gradle-props-denied",
        "gradle-init-denied",
        "codex-cfg-denied",
        "codex-hooks-denied",
    ] {
        assert!(log.contains(marker), "missing {marker}: {log}");
    }
}
