#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for audit round 2 fixes across throttle, sandbox,
//! executors, stream log scrubbing, and multi-host hook settings.

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};
use std::time::Duration;

use aaf_domain::ports::SandboxPolicy;
use aaf_engine::executors::linux_sandbox::build_bwrap_args;
use aaf_engine::executors::sandbox::{SandboxPaths, seatbelt_profile};
use aaf_engine::executors::secret_mask::{NamedSecret, mask_named_and_patterns};
use aaf_engine::executors::stream_log::scrub_secrets;
use aaf_engine::hook_settings::multi_host::{
    CODEX_HOOKS_REL, GEMINI_SETTINGS_REL, covering_gemini_hook, gemini_matcher, register_codex,
    register_gemini,
};
use aaf_engine::throttle::Sleeper;
use aaf_engine::throttle::heavy::{
    HeavyTool, heavy_tool, heavy_tool_argv, stats_command_family, stats_command_family_argv,
};
use aaf_engine::throttle::paths::throttle_dir;
#[cfg(unix)]
use aaf_engine::throttle::paths::{drain_path, is_guest};
use aaf_engine::throttle::queue::{WaitTicket, older_waiters};
use aaf_engine::throttle::stats::{HeavySample, cmd_label, record, recorded_peak_for, tool_label};
use serde_json::json;
use tempfile::TempDir;

struct NoopSleeper;

impl Sleeper for NoopSleeper {
    fn sleep(&self, _duration: Duration) {}
}

fn sample(peak_rss_bytes: u64) -> HeavySample {
    HeavySample {
        peak_rss_bytes,
        duration_ms: 1_000,
    }
}

#[test]
fn recorded_peak_for_does_not_fallback_to_cmd_family_when_tool_is_some() {
    let data = TempDir::new().unwrap();
    let sleeper = NoopSleeper;
    let gib = 1024 * 1024 * 1024;

    // Only `@cmd:gradle` and `@cmd:cargo` exist; `@tool:gradle` has never been recorded.
    assert!(
        record(
            data.path(),
            "repo",
            &cmd_label("gradle"),
            sample(2 * gib),
            &sleeper
        )
        .unwrap()
    );
    assert!(
        record(
            data.path(),
            "repo",
            &cmd_label("cargo"),
            sample(gib),
            &sleeper
        )
        .unwrap()
    );

    // Daemon-heavy tool (`tool = Some(Gradle)`) must NOT fall back to `@cmd:gradle`.
    assert_eq!(
        recorded_peak_for(
            data.path(),
            "repo",
            "custom-label",
            Some(HeavyTool::Gradle),
            Some("gradle"),
        ),
        None
    );

    // Non-daemon command (`tool = None`) still reuses `@cmd:cargo`.
    assert_eq!(
        recorded_peak_for(data.path(), "repo", "custom-label", None, Some("cargo")),
        Some(gib)
    );

    // Once `@tool:gradle` is recorded, `tool = Some(Gradle)` uses `@tool:gradle`, not `@cmd:gradle`.
    assert!(
        record(
            data.path(),
            "repo",
            &tool_label(HeavyTool::Gradle),
            sample(4 * gib),
            &sleeper,
        )
        .unwrap()
    );
    assert_eq!(
        recorded_peak_for(
            data.path(),
            "repo",
            "custom-label",
            Some(HeavyTool::Gradle),
            Some("gradle"),
        ),
        Some(4 * gib)
    );
}

#[test]
fn queue_atomic_join_leaves_no_tmp_and_guests_count_older_waiters_read_only() {
    let data = TempDir::new().unwrap();
    let first = WaitTicket::join(data.path(), 1_000).unwrap();

    // Atomic join creates `.wait` via `.tmp` rename and leaves zero `.tmp` files behind.
    let queue_dir = throttle_dir(data.path()).join("queue");
    let entries: Vec<String> = fs::read_dir(&queue_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, vec![first.name().to_string()]);
    assert!(entries.iter().all(|name| !name.ends_with(".tmp")));

    // Plant an older unlocked stale ticket file.
    let stale = queue_dir.join("0000000000000500-00000001-00000000.wait");
    fs::write(&stale, "").unwrap();

    #[cfg(unix)]
    {
        // Simulate a sandboxed guest by making `throttle/drain.lock` read-only.
        let drain = drain_path(data.path());
        fs::write(&drain, "").unwrap();
        fs::set_permissions(&drain, fs::Permissions::from_mode(0o400)).unwrap();

        if is_guest(data.path()) {
            // Guest cannot join the queue, MUST still see the live host waiter via
            // read-only shared lock probe, and must NOT remove stale files.
            assert!(WaitTicket::join(data.path(), 1_005).is_none());
            assert_eq!(older_waiters(data.path(), None), 1);
            assert!(stale.exists(), "guest must not unlink stale tickets");
        }

        // Restore write permission (host mode) so host cleanup and TempDir drop succeed.
        fs::set_permissions(&drain, fs::Permissions::from_mode(0o600)).unwrap();
    }

    // Host sees the live waiter and cleans up the stale unlocked ticket.
    assert_eq!(older_waiters(data.path(), None), 1);
    assert!(!stale.exists(), "host must unlink stale tickets");

    drop(first);
    assert_eq!(older_waiters(data.path(), None), 0);
}

#[test]
fn heavy_tool_recognizes_taskpolicy_valued_flags_and_family_reexports() {
    for cmd in [
        "taskpolicy -P kill ./gradlew test",
        "taskpolicy -S 1 ./gradlew test",
        "taskpolicy -m 2 ./gradlew test",
        "taskpolicy -j 3 ./gradlew test",
        "taskpolicy -p 12345 ./gradlew test",
        "taskpolicy -c utility -P kill ./gradlew test",
    ] {
        assert_eq!(heavy_tool(cmd), Some(HeavyTool::Gradle), "{cmd:?}");
    }
    assert_eq!(
        heavy_tool_argv(&["taskpolicy", "-P", "kill", "./gradlew", "test"]),
        Some(HeavyTool::Gradle)
    );
    // Option values named like a build tool must not trigger a false positive.
    assert_eq!(heavy_tool("taskpolicy -P gradle true"), None);
    assert_eq!(
        heavy_tool_argv(&["taskpolicy", "-P", "gradle", "true"]),
        None
    );

    // Re-exports from `throttle::heavy` remain intact after extracting `heavy/family.rs`.
    assert_eq!(
        stats_command_family("cargo test --workspace"),
        Some("cargo")
    );
    assert_eq!(
        stats_command_family_argv(&["taskpolicy", "-P", "kill", "cargo", "build"]),
        Some("cargo")
    );
}

#[test]
fn scrub_secrets_preserves_bearer_named_secret_placeholder() {
    // Previously a redundant second regex pass turned `Bearer [secret NAME]`
    // into `Bearer [REDACTED] NAME]`.
    let already_masked = "Authorization: Bearer [secret MY_API_TOKEN]";
    assert_eq!(scrub_secrets(already_masked, &[]), already_masked);

    // End-to-end: masking a named env secret inside a Bearer header followed by
    // `scrub_secrets` must keep the named placeholder intact without leaking the value.
    let token_val = ["tok_live_", "9876543210abcdef"].concat();
    let named = vec![NamedSecret::new("MY_API_TOKEN", token_val.clone())];
    let header = format!("Authorization: Bearer {token_val}");
    let masked = mask_named_and_patterns(&header, &named, &[]);
    assert_eq!(masked, already_masked);
    assert_eq!(scrub_secrets(&masked, &[]), already_masked);

    // Unrecognized raw Bearer tokens are still redacted.
    let raw_bearer = format!(
        "Authorization: Bearer {}",
        ["raw_", "bearer_xyz123"].concat()
    );
    assert_eq!(
        scrub_secrets(&raw_bearer, &[]),
        "Authorization: Bearer [REDACTED]"
    );
}

#[test]
fn sandbox_denies_git_config_worktree_and_canonicalizes_run_mark() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let git_dir = root.join("repo.git");
    fs::create_dir_all(git_dir.join("hooks")).unwrap();
    fs::write(git_dir.join("config"), "[core]\n").unwrap();
    fs::write(git_dir.join("config.worktree"), "[core]\n").unwrap();

    // Seatbelt profile denies `<git_dir>/config.worktree` alongside `config`.
    let extra = SandboxPaths {
        git_dirs: vec![git_dir.to_string_lossy().into_owned()],
        ..SandboxPaths::default()
    };
    let profile = seatbelt_profile(&SandboxPolicy::default(), &extra).unwrap();
    let expected_wt = format!("(literal \"{}/config.worktree\")", git_dir.display());
    assert!(profile.contains(&expected_wt), "{profile}");

    // Linux Bubblewrap args also bind `<git_dir>/config.worktree` read-only.
    let bwrap = build_bwrap_args(&extra, "/bin/true", &[]).unwrap();
    let wt_cfg = git_dir
        .join("config.worktree")
        .to_string_lossy()
        .into_owned();
    assert!(
        bwrap
            .windows(3)
            .any(|w| w[0] == "--ro-bind" && w[1] == wt_cfg && w[2] == wt_cfg),
        "missing --ro-bind for config.worktree in {bwrap:?}"
    );

    // Symlinked `run_mark` (e.g. via `/tmp` or a symlinked `AAF_DATA_DIR`) is canonicalized.
    #[cfg(unix)]
    {
        let real_data = root.join("real-data");
        let link_data = root.join("link-data");
        fs::create_dir_all(real_data.join("marks")).unwrap();
        symlink(&real_data, &link_data).unwrap();
        let mark_via_link = link_data.join("marks/run-42.mark");
        fs::write(&mark_via_link, "").unwrap();

        let extra_mark = SandboxPaths {
            run_mark: Some(mark_via_link.to_string_lossy().into_owned()),
            ..SandboxPaths::default()
        };
        let profile = seatbelt_profile(&SandboxPolicy::default(), &extra_mark).unwrap();
        let expected_mark = format!(
            "(allow file-write-data (literal \"{}/marks/run-42.mark\"))\n",
            real_data.display()
        );
        assert!(profile.ends_with(&expected_mark), "{profile}");
        assert!(!profile.contains("/link-data/"), "{profile}");
    }
}

#[test]
fn gemini_hooks_disabled_fails_closed_on_non_object_and_register_writes_atomically() {
    for bad_config in [
        json!(false),
        json!(true),
        json!(null),
        json!([]),
        json!("disabled"),
        json!(0),
    ] {
        let cfg = json!({
            "hooksConfig": bad_config,
            "hooks": {
                "BeforeTool": [{
                    "matcher": gemini_matcher(),
                    "hooks": [{"type": "command", "command": "aaf-hook --host gemini"}],
                }]
            }
        });
        assert!(
            covering_gemini_hook(&cfg.to_string()).is_err(),
            "non-object hooksConfig={bad_config} must fail closed"
        );
    }

    // Object without `enabled` or with `enabled: true` stays valid.
    for ok_config in [json!({}), json!({"enabled": true})] {
        let cfg = json!({
            "hooksConfig": ok_config,
            "hooks": {
                "BeforeTool": [{
                    "matcher": gemini_matcher(),
                    "hooks": [{"type": "command", "command": "aaf-hook --host gemini"}],
                }]
            }
        });
        assert!(
            covering_gemini_hook(&cfg.to_string()).is_ok(),
            "valid hooksConfig={ok_config} should be accepted"
        );
    }

    // Atomic registration writes final config and leaves no `.aaf-tmp-*` files behind.
    let dir = TempDir::new().unwrap();
    register_codex(dir.path(), "aaf-hook").unwrap();
    register_gemini(dir.path(), "aaf-hook").unwrap();
    for rel in [CODEX_HOOKS_REL, GEMINI_SETTINGS_REL] {
        let parent = dir.path().join(rel).parent().unwrap().to_path_buf();
        let names: Vec<String> = fs::read_dir(&parent)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().all(|name| !name.contains(".aaf-tmp-")),
            "leftover temp file in {parent:?}: {names:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn claude_runner_clears_limit_hit_when_run_times_out() {
    use aaf_domain::ports::{AgentRequest, AgentRunner};
    use aaf_engine::executors::ExecEnv;
    use aaf_engine::executors::claude::ClaudeCodeRunner;

    let dir = TempDir::new().unwrap();
    let work = dir.path().join("work");
    fs::create_dir_all(&work).unwrap();

    let fake_bin = dir.path().join("fake-claude-timeout");
    let script = concat!(
        "#!/bin/sh\n",
        "printf '%s\\n' '{\"type\":\"rate_limit_event\",\"rate_limit_info\":{\"status\":\"rejected\",\"resetsAt\":1999999999}}'\n",
        "sleep 10\n",
    );
    fs::write(&fake_bin, script).unwrap();
    fs::set_permissions(&fake_bin, fs::Permissions::from_mode(0o755)).unwrap();

    let runner = ClaudeCodeRunner::new(fake_bin.to_string_lossy(), ExecEnv::default());
    let outcome = runner
        .run(&AgentRequest {
            run_id: format!("test-timeout-limit-{}", std::process::id()),
            prompt: "work".to_string(),
            cwd: work.to_string_lossy().into_owned(),
            account: None,
            timeout_secs: 1,
            sandbox: SandboxPolicy::default(),
        })
        .unwrap();

    assert!(outcome.timed_out, "{outcome:?}");
    assert_eq!(
        outcome.limit_hit, None,
        "timed-out run must not report limit_hit"
    );
}

#[test]
fn pools_share_guest_inode_between_host_and_guest_and_reject_links() {
    use aaf_domain::ports::Clock;
    use aaf_engine::throttle::paths::pools_dir;
    use aaf_engine::throttle::pools::{CorrectnessAsk, acquire_correctness};
    use aaf_engine::throttle::slots::prepare_for_guests;

    struct FixedClock(i64);
    impl Clock for FixedClock {
        fn now(&self) -> i64 {
            self.0
        }
    }

    let data = TempDir::new().unwrap();
    let wt1 = TempDir::new().unwrap();
    let wt2 = TempDir::new().unwrap();
    let clock = FixedClock(1_000);
    let sleeper = NoopSleeper;
    fn ask(tool: HeavyTool, cwd: &std::path::Path) -> CorrectnessAsk<'_> {
        CorrectnessAsk {
            tool: Some(tool),
            cwd,
            max_unity_parallel: 1,
            wait_limit_secs: Some(0),
            jitter_seed: 7,
        }
    }

    prepare_for_guests(data.path(), 2, &|_| None).unwrap();
    assert!(pools_dir(data.path()).is_dir());
    assert!(!throttle_dir(data.path()).join("locks").exists());

    let host_guard = acquire_correctness(
        data.path(),
        &ask(HeavyTool::Unity, wt1.path()),
        &clock,
        &sleeper,
    )
    .unwrap();
    assert!(host_guard.is_some());

    #[cfg(unix)]
    {
        let drain = drain_path(data.path());
        fs::set_permissions(&drain, fs::Permissions::from_mode(0o400)).unwrap();
        if is_guest(data.path()) {
            // Guest contends on the exact same Unity and Worktree lock inodes.
            assert!(
                acquire_correctness(
                    data.path(),
                    &ask(HeavyTool::Unity, wt2.path()),
                    &clock,
                    &sleeper
                )
                .unwrap()
                .is_none()
            );
            assert!(
                acquire_correctness(
                    data.path(),
                    &ask(HeavyTool::Gradle, wt1.path()),
                    &clock,
                    &sleeper
                )
                .unwrap()
                .is_none()
            );
            assert!(
                acquire_correctness(
                    data.path(),
                    &ask(HeavyTool::Gradle, wt2.path()),
                    &clock,
                    &sleeper
                )
                .unwrap()
                .is_some()
            );
        }
        fs::set_permissions(&drain, fs::Permissions::from_mode(0o600)).unwrap();
    }
    drop(host_guard);

    // Symlink or hardlink planted in `pools/` fails closed.
    #[cfg(unix)]
    {
        let target = data.path().join("outside-target");
        fs::write(&target, "keep").unwrap();
        let unity_lock = pools_dir(data.path()).join("tool.unity.0.lock");
        let _ = fs::remove_file(&unity_lock);
        symlink(&target, &unity_lock).unwrap();
        assert!(
            acquire_correctness(
                data.path(),
                &ask(HeavyTool::Unity, wt1.path()),
                &clock,
                &sleeper
            )
            .is_err()
        );
        fs::remove_file(&unity_lock).unwrap();
        fs::hard_link(&target, &unity_lock).unwrap();
        assert!(
            acquire_correctness(
                data.path(),
                &ask(HeavyTool::Unity, wt1.path()),
                &clock,
                &sleeper
            )
            .is_err()
        );
    }
}

#[test]
fn seatbelt_profile_honors_policy_read_only_on_policy_writable_paths() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let policy_dir = root.join("policy-rw");
    let extra_dir = root.join("extra-rw");
    fs::create_dir_all(&policy_dir).unwrap();
    fs::create_dir_all(&extra_dir).unwrap();

    let ro_policy = SandboxPolicy {
        enabled: true,
        read_only: true,
        writable_paths: vec![policy_dir.to_string_lossy().into_owned()],
        read_denied_paths: Vec::new(),
    };
    let extra = SandboxPaths {
        writable: vec![extra_dir.to_string_lossy().into_owned()],
        ..SandboxPaths::default()
    };

    let ro_profile = seatbelt_profile(&ro_policy, &extra).unwrap();
    let policy_rule = format!("(allow file-write* (subpath \"{}\"))", policy_dir.display());
    let extra_rule = format!("(allow file-write* (subpath \"{}\"))", extra_dir.display());
    assert!(
        !ro_profile.contains(&policy_rule),
        "read_only policy must ignore policy.writable_paths: {ro_profile}"
    );
    assert!(ro_profile.contains(&extra_rule), "{ro_profile}");

    // When read_only is false, policy.writable_paths IS included.
    let rw_policy = SandboxPolicy {
        read_only: false,
        ..ro_policy
    };
    let rw_profile = seatbelt_profile(&rw_policy, &extra).unwrap();
    assert!(rw_profile.contains(&policy_rule), "{rw_profile}");
    assert!(rw_profile.contains(&extra_rule), "{rw_profile}");
}
