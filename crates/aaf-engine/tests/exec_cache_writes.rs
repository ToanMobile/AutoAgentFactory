#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(target_os = "macos")]
//! Wave 3, from real builds of the owner's repositories: which tool caches a
//! confined run may write (Kotlin/Native, Unity, Xcode, the Android SDK), and
//! which files inside the writable caches and the account directory stay
//! closed because a later build or shell outside the sandbox runs or trusts
//! them. The real `sandbox-exec` with a stand-in `claude` that probes.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use aaf_domain::account::{AccountProfile, ProviderKind};
use aaf_domain::ports::{AgentRequest, AgentRunner, SandboxPolicy};
use aaf_engine::executors::ExecEnv;
use aaf_engine::executors::claude::ClaudeCodeRunner;

#[path = "../../aaf-fp/tests/exec_support.rs"]
mod exec_support;
use exec_support::write_executable;

/// Caches builds write: `(marker, path below the home directory)`.
const OPEN: [(&str, &str); 14] = [
    ("konan", ".konan/kotlin-native/x"),
    (
        "kotlin-daemon",
        "Library/Application Support/kotlin/daemon/x",
    ),
    ("unity-lib", "Library/Unity/x"),
    (
        "unity-editor-cache",
        "Library/Caches/com.unity3d.UnityEditor/x",
    ),
    ("unity-cache", "Library/Caches/Unity/x"),
    ("unity-logs", "Library/Logs/Unity/Editor.log"),
    ("unity-support", "Library/Application Support/Unity/x"),
    ("coresimulator", "Library/Developer/CoreSimulator/Devices/x"),
    ("xcode-cache", "Library/Caches/com.apple.dt.Xcode/x"),
    ("swiftpm-lib", "Library/org.swift.swiftpm/x"),
    ("gradle-caches", ".gradle/caches/x"),
    ("cargo-registry", ".cargo/registry/x"),
    ("m2-repository", ".m2/repository/x"),
    ("android-cache", ".android/cache/x"),
];

/// Inside those caches, but run or trusted outside the sandbox later.
const CLOSED: [(&str, &str); 16] = [
    ("gradle-initd", ".gradle/init.d/evil.gradle"),
    ("gradle-properties", ".gradle/gradle.properties"),
    ("gradle-init", ".gradle/init.gradle"),
    ("gradle-init-kts", ".gradle/init.gradle.kts"),
    ("cargo-bin", ".cargo/bin/cargo-evil"),
    ("cargo-config", ".cargo/config.toml"),
    ("cargo-config-old", ".cargo/config"),
    ("cargo-credentials", ".cargo/credentials.toml"),
    ("cargo-env", ".cargo/env"),
    ("adbkey", ".android/adbkey"),
    ("adbkey-pub", ".android/adbkey.pub"),
    ("debug-keystore", ".android/debug.keystore"),
    ("m2-settings", ".m2/settings.xml"),
    ("m2-settings-security", ".m2/settings-security.xml"),
    ("pub-bin", ".pub-cache/bin/evil"),
    ("pub-credentials", ".pub-cache/credentials.json"),
];

fn probe(marker: &str, path: &Path) -> String {
    format!(
        "( echo probe > '{}' ) 2>/dev/null && echo {marker}-ok || echo {marker}-denied\n",
        path.display()
    )
}

struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    sdk: PathBuf,
    account: PathBuf,
    shared: PathBuf,
}

fn world() -> World {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let world = World {
        home: root.join("home"),
        sdk: root.join("sdk"),
        account: root.join("acct"),
        shared: root.join("shared"),
        root,
        _tmp: tmp,
    };
    for (_, rel) in OPEN.iter().chain(&CLOSED) {
        fs::create_dir_all(world.home.join(rel).parent().unwrap()).unwrap();
    }
    for dir in [
        world.sdk.join("platforms"),
        world.sdk.join("platform-tools"),
        world.account.join("projects"),
        world.shared.join("hooks"),
        world.root.join("wt"),
    ] {
        fs::create_dir_all(dir).unwrap();
    }
    world
}

fn run(world: &World, script: &str) -> String {
    let claude = world.root.join("claude");
    write_executable(&claude, format!("#!/bin/sh\n{script}"));
    let env = ExecEnv {
        home: Some(world.home.to_string_lossy().into_owned()),
        // Nothing writable by default: the temp dir holds this whole tree.
        temp_dirs: Vec::new(),
        sdk_dirs: vec![world.sdk.to_string_lossy().into_owned()],
        parent_env: vec![("PATH".to_string(), "/usr/bin:/bin".to_string())],
        ..ExecEnv::default()
    };
    ClaudeCodeRunner::new(claude.to_string_lossy(), env)
        .run(&AgentRequest {
            run_id: format!("test-{}", uuid::Uuid::new_v4()),
            prompt: "unused".to_string(),
            cwd: world.root.join("wt").to_string_lossy().into_owned(),
            account: Some(AccountProfile {
                id: "claude_code-test".to_string(),
                provider: ProviderKind::ClaudeCode,
                config_dir: world.account.to_string_lossy().into_owned(),
                name: "test".to_string(),
            }),
            timeout_secs: 60,
            sandbox: SandboxPolicy {
                enabled: true,
                ..SandboxPolicy::default()
            },
            json_schema: None,
            max_turns: None,
        })
        .unwrap()
        .log_tail
}

#[test]
fn build_caches_are_writable_and_what_runs_outside_the_sandbox_is_not() {
    if !Path::new("/usr/bin/sandbox-exec").exists() {
        return;
    }
    let world = world();
    fs::write(world.home.join(".gradle/gradle.properties"), "kept\n").unwrap();
    let mut script = String::new();
    for (marker, rel) in OPEN.iter().chain(&CLOSED) {
        script.push_str(&probe(marker, &world.home.join(rel)));
    }
    script.push_str(&probe("sdk", &world.sdk.join("platforms/x")));
    script.push_str(&probe("sdk-adb", &world.sdk.join("platform-tools/adb")));
    // Moving a closed entry aside (to put another in its place) and deleting
    // a closed file are writes to it as well.
    let gradle = world.home.join(".gradle");
    script.push_str(&format!(
        "mv '{0}/init.d' '{0}/caches/init.d' 2>/dev/null && echo initd-moved\n\
         rm -f '{0}/gradle.properties' 2>/dev/null\n",
        gradle.display()
    ));

    let log = run(&world, &script);

    for (marker, _) in OPEN {
        assert!(log.contains(&format!("{marker}-ok")), "{marker}: {log}");
    }
    assert!(log.contains("sdk-ok"), "{log}");
    for (marker, _) in CLOSED {
        assert!(log.contains(&format!("{marker}-denied")), "{marker}: {log}");
    }
    assert!(log.contains("sdk-adb-denied"), "{log}");
    assert!(!log.contains("initd-moved"), "{log}");
    assert!(gradle.join("init.d").is_dir());
    assert_eq!(
        fs::read_to_string(gradle.join("gradle.properties")).unwrap(),
        "kept\n"
    );
}

#[test]
fn the_accounts_own_settings_and_hooks_are_closed_and_its_state_is_open() {
    if !Path::new("/usr/bin/sandbox-exec").exists() {
        return;
    }
    let world = world();
    let account = &world.account;
    // Not links into a shared directory: files of the account itself, which
    // its sessions outside the sandbox obey.
    fs::write(account.join("settings.json"), "{}").unwrap();
    symlink(world.shared.join("hooks"), account.join("hooks")).unwrap();
    let mut script = String::new();
    script.push_str(&probe("settings", &account.join("settings.json")));
    script.push_str(&probe(
        "settings-local",
        &account.join("settings.local.json"),
    ));
    script.push_str(&probe("hook", &account.join("hooks/h.sh")));
    script.push_str(&probe(
        "transcript",
        &account.join("projects/session.jsonl"),
    ));
    // Claude Code rewrites this file in every session; see the report.
    script.push_str(&probe("claude-json", &account.join(".claude.json")));
    // Replacing the link with a directory of the agent's own.
    script.push_str(&format!(
        "rm '{0}/hooks' 2>/dev/null && mkdir '{0}/hooks' && echo hooks-relinked\n",
        account.display()
    ));

    let log = run(&world, &script);

    for marker in [
        "settings-denied",
        "settings-local-denied",
        "hook-denied",
        "transcript-ok",
        "claude-json-ok",
    ] {
        assert!(log.contains(marker), "missing {marker}: {log}");
    }
    assert!(!log.contains("hooks-relinked"), "{log}");
    assert_eq!(
        fs::read_to_string(account.join("settings.json")).unwrap(),
        "{}"
    );
    assert!(
        fs::symlink_metadata(account.join("hooks"))
            .unwrap()
            .is_symlink()
    );
    assert!(!world.shared.join("hooks/h.sh").exists());
}
