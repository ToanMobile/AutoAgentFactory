#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Proves the heavy-command invariant across `aaf-engine` and `aaf-hook`:
//! 1. Mobile build commands (`flutter build`, `expo run:*`, `react-native run-*`,
//!    `pod install`) are classified as `HeavyTool::Mobile` by the engine and
//!    denied by `aaf-hook` outside `aaf slot`.
//! 2. Light commands (`flutter test`, `flutter analyze`, `expo start`,
//!    `pod --version`, etc.) are allowed by both.
//! 3. `engine ⊇ hook`: every command rejected by `aaf-hook`'s `heavy_cmd_guard`
//!    is classified as `Some(_)` by `aaf_engine::throttle::heavy_tool`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use aaf_domain::heavy::{GRADLE_PROGRAMS, UNITY_HEAVY_FLAGS, UNITY_PROGRAMS, XCODEBUILD_PROGRAMS};
use aaf_engine::throttle::heavy::{HEAVY_FLAGS, HEAVY_PROGRAMS};
use aaf_engine::throttle::{HeavyTool, heavy_tool, heavy_tool_argv, unqueued_heavy_tool};
use serde_json::json;

fn aaf_hook_bin() -> PathBuf {
    let sibling = Path::new(env!("CARGO_BIN_EXE_aaf")).with_file_name("aaf-hook");
    assert!(
        sibling.is_file(),
        "aaf-hook binary not found at {} (run `cargo build -p aaf-hook --bin aaf-hook`)",
        sibling.display()
    );
    sibling
}

fn hook_denies_heavy(command: &str) -> bool {
    let payload = json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": { "command": command }
    })
    .to_string();
    let mut child = Command::new(aaf_hook_bin())
        .env("AAF_RUN_ID", "parity-check-1")
        .env("AAF_DATA_DIR", "/tmp/aaf-parity-data")
        .env_remove("AAF_HEAVY_SLOT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    !out.status.success()
        && (stderr.contains("heavy_cmd_guard") || stdout.contains("heavy_cmd_guard"))
}

#[test]
fn mobile_heavy_subcommands_are_classified_and_guarded() {
    let heavy_cases = [
        "flutter build apk",
        "flutter --no-version-check build ios --release",
        "flutter -- build apk",
        "expo run:ios",
        "expo run:android --variant release",
        "npx expo run:ios",
        "npx -y expo run:android",
        "npx --package @expo/cli expo run:ios",
        "npm exec expo run:ios",
        "npm exec -- expo run:ios",
        "bunx expo run:ios",
        "bun x react-native run-ios",
        "yarn expo run:ios",
        "yarn --cwd apps/mobile expo run:ios",
        "pnpm exec expo run:android",
        "pnpm -C apps/mobile exec expo run:android",
        "react-native run-ios",
        "npx react-native run-android",
        "pod install",
        "pod --verbose install",
        "bundle exec pod install",
        "rbenv exec pod install",
        "direnv exec . pod install",
        "mise exec -- pod install",
        "mise -C ios exec -- flutter build ipa",
    ];
    for cmd in heavy_cases {
        assert_eq!(
            heavy_tool(cmd),
            Some(HeavyTool::Mobile),
            "engine missed mobile heavy command: {cmd}"
        );
        let argv: Vec<&str> = cmd.split_whitespace().collect();
        assert_eq!(
            heavy_tool_argv(&argv),
            Some(HeavyTool::Mobile),
            "engine argv missed mobile heavy command: {cmd}"
        );
        assert!(
            hook_denies_heavy(cmd),
            "aaf-hook failed to deny mobile heavy command: {cmd}"
        );
    }
    for quoted in [
        "npx -c 'expo run:ios'",
        "sh -c 'flutter build apk'",
        "cd ios && pod install",
    ] {
        assert_eq!(
            heavy_tool(quoted),
            Some(HeavyTool::Mobile),
            "engine missed shell-quoted mobile heavy command: {quoted}"
        );
        assert!(
            hook_denies_heavy(quoted),
            "aaf-hook failed to deny shell-quoted mobile heavy command: {quoted}"
        );
    }
    assert_eq!(unqueued_heavy_tool("aaf slot -- flutter build apk"), None);
    assert_eq!(
        heavy_tool("aaf slot -- flutter build apk"),
        Some(HeavyTool::Mobile)
    );
}

#[test]
fn mobile_light_commands_are_not_misclassified() {
    let light_cases = [
        "flutter test",
        "flutter analyze",
        "flutter pub get",
        "flutter --version",
        "expo start",
        "npx expo doctor",
        "npm run build",
        "yarn run build",
        "react-native info",
        "pod --version",
        "pod env",
        "echo flutter build apk",
        "echo pod install",
    ];
    for cmd in light_cases {
        assert_eq!(
            heavy_tool(cmd),
            None,
            "engine false-positive on light command: {cmd}"
        );
        assert!(
            !hook_denies_heavy(cmd),
            "aaf-hook false-positive on light command: {cmd}"
        );
    }
}

#[test]
fn engine_tables_contain_all_shared_domain_heavy_entries() {
    for &prog in GRADLE_PROGRAMS
        .iter()
        .chain(XCODEBUILD_PROGRAMS)
        .chain(UNITY_PROGRAMS)
    {
        assert!(
            HEAVY_PROGRAMS
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case(prog)),
            "engine HEAVY_PROGRAMS missing shared domain entry: {prog}"
        );
    }
    for &flag in UNITY_HEAVY_FLAGS {
        assert!(
            HEAVY_FLAGS
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case(flag)),
            "engine HEAVY_FLAGS missing shared domain entry: {flag}"
        );
    }
}

#[test]
fn engine_heavy_classifier_is_a_superset_of_hook_heavy_guard() {
    let matrix = [
        "./gradlew testDebugUnitTest",
        "gradle --version",
        "xcodebuild -project Demo.xcodeproj -scheme Demo build",
        "xcodebuild -version",
        "Unity -batchmode -runTests",
        "unity -executeMethod Build.Run",
        "flutter build apk",
        "flutter test",
        "npx expo run:ios",
        "npx expo start",
        "npx react-native run-android",
        "pod install",
        "bundle exec pod install",
        "pod --version",
        "cargo test --workspace",
        "go test ./...",
        "pytest",
    ];
    for cmd in matrix {
        let hook_blocks = hook_denies_heavy(cmd);
        let engine_class = heavy_tool(cmd);
        if hook_blocks {
            assert!(
                engine_class.is_some(),
                "invariant violated (engine must be a superset of hook): \
                 aaf-hook denied `{cmd}` as heavy_cmd_guard, but engine returned None"
            );
        }
    }
}
