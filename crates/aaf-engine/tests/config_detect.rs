#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Stack detection on temp-dir fixtures and the `aaf init` config rendering.

use std::fs;
use std::path::Path;

use aaf_domain::config::{CommandsConfig, RepoConfig};
use aaf_engine::config::{Detected, detect_stack, effective_profile, render_repo_config};
use tempfile::TempDir;

fn repo(files: &[(&str, &str)]) -> TempDir {
    let dir = TempDir::new().unwrap();
    for (rel, body) in files {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
    dir
}

fn detect(files: &[(&str, &str)]) -> Detected {
    detect_stack(repo(files).path())
}

const RN_PACKAGE: &str = r#"{"dependencies": {"react": "19.0.0", "react-native": "0.80.0"}}"#;
const WEB_PACKAGE: &str = r#"{"dependencies": {"react": "19.0.0"}, "scripts": {"test": "vitest"}}"#;

#[test]
fn empty_repo_is_auto_with_no_commands_and_no_evidence() {
    let d = detect(&[]);
    assert_eq!(d.profile, "auto");
    assert_eq!(d.commands, CommandsConfig::default());
    assert!(d.evidence.is_empty());
}

#[test]
fn rust_by_cargo_toml_with_certain_commands() {
    let d = detect(&[("Cargo.toml", "[package]\nname = \"x\"\n")]);
    assert_eq!(d.profile, "rust");
    assert_eq!(d.commands.build, "cargo build");
    assert_eq!(d.commands.test_scoped, "cargo test {affected_packages}");
    assert_eq!(d.commands.test_full, "cargo test --workspace");
    assert_eq!(
        d.commands.lint,
        "cargo clippy {affected_packages} --all-targets -- -D warnings"
    );
    assert_eq!(d.evidence, vec!["Cargo.toml"]);
}

#[test]
fn android_needs_gradle_and_manifest() {
    let d = detect(&[
        ("build.gradle.kts", ""),
        ("app/src/main/AndroidManifest.xml", "<manifest/>"),
    ]);
    assert_eq!(d.profile, "android");
    assert_eq!(
        d.evidence,
        vec!["build.gradle.kts", "app/src/main/AndroidManifest.xml"]
    );
    // No wrapper script: no command is certain.
    assert_eq!(d.commands, CommandsConfig::default());

    // Gradle alone (a JVM library or service) is not an Android app.
    assert_eq!(detect(&[("build.gradle", "")]).profile, "auto");
    // A manifest alone is not a buildable Android project either.
    assert_eq!(
        detect(&[("app/src/main/AndroidManifest.xml", "<manifest/>")]).profile,
        "auto"
    );
}

#[test]
fn android_manifest_is_found_in_a_non_default_module() {
    let d = detect(&[
        ("build.gradle", ""),
        ("mobile/src/main/AndroidManifest.xml", "<manifest/>"),
    ]);
    assert_eq!(d.profile, "android");
    assert_eq!(
        d.evidence,
        vec!["build.gradle", "mobile/src/main/AndroidManifest.xml"]
    );
}

#[test]
fn android_commands_only_with_gradlew() {
    let d = detect(&[
        ("build.gradle", ""),
        ("gradlew", "#!/bin/sh\n"),
        ("app/src/main/AndroidManifest.xml", "<manifest/>"),
    ]);
    assert_eq!(d.profile, "android");
    assert_eq!(d.commands.build, "./gradlew assembleDebug --no-daemon");
    assert_eq!(
        d.commands.test_scoped,
        "./gradlew testDebugUnitTest --no-daemon"
    );
    assert_eq!(
        d.commands.test_full,
        "./gradlew testDebugUnitTest --no-daemon"
    );
    assert_eq!(d.commands.lint, "");
    assert_eq!(d.commands.lint_full, "./gradlew lint --no-daemon");
    assert!(d.evidence.contains(&"gradlew".to_string()));
}

#[test]
fn ios_by_xcodeproj_xcworkspace_or_package_swift() {
    let d = detect(&[("Demo.xcodeproj/project.pbxproj", "")]);
    assert_eq!(d.profile, "ios");
    assert_eq!(d.evidence, vec!["Demo.xcodeproj"]);
    assert_eq!(d.commands, CommandsConfig::default());

    let ws = detect(&[("Demo.xcworkspace/contents.xcworkspacedata", "")]);
    assert_eq!(ws.profile, "ios");
    assert_eq!(ws.evidence, vec!["Demo.xcworkspace"]);

    let spm = detect(&[("Package.swift", "// swift-tools-version: 5.9\n")]);
    assert_eq!(spm.profile, "ios");
    assert_eq!(spm.evidence, vec!["Package.swift"]);
}

#[test]
fn flutter_by_pubspec() {
    let pubspec = "name: demo\ndependencies:\n  flutter:\n    sdk: flutter\n";
    let with_tests = detect(&[
        ("pubspec.yaml", pubspec),
        ("test/widget_test.dart", "void main() {}\n"),
        // A Flutter app carries native shells; they must not win.
        ("android/build.gradle", ""),
        ("android/app/src/main/AndroidManifest.xml", "<manifest/>"),
        ("ios/Runner.xcodeproj/project.pbxproj", ""),
    ]);
    assert_eq!(with_tests.profile, "flutter");
    assert_eq!(with_tests.commands.test_scoped, "flutter test");
    assert_eq!(with_tests.commands.test_full, "flutter test");
    assert_eq!(with_tests.commands.lint, "flutter analyze");
    assert_eq!(with_tests.commands.build, "");
    assert_eq!(with_tests.evidence, vec!["pubspec.yaml", "test/"]);

    // Without a `test/` directory, `flutter test` would fail; leave test commands empty.
    let without_tests = detect(&[
        ("pubspec.yaml", pubspec),
        ("android/build.gradle", ""),
        ("android/app/src/main/AndroidManifest.xml", "<manifest/>"),
    ]);
    assert_eq!(without_tests.profile, "flutter");
    assert_eq!(without_tests.commands.test_scoped, "");
    assert_eq!(without_tests.commands.test_full, "");
    assert_eq!(without_tests.commands.lint, "flutter analyze");
    assert_eq!(without_tests.commands.build, "");
    assert_eq!(
        without_tests.evidence,
        vec!["pubspec.yaml (no test/ directory)"]
    );

    // A plain Dart package without `sdk: flutter` falls through to `auto`.
    let plain = detect(&[("pubspec.yaml", "name: demo\n")]);
    assert_eq!(plain.profile, "auto");
    assert_eq!(plain.commands, CommandsConfig::default());
}

#[test]
fn expo_by_app_json_config_file_or_dependency_wins_over_react_native() {
    let d = detect(&[
        ("app.json", r#"{"expo": {"name": "demo"}}"#),
        ("package.json", RN_PACKAGE),
    ]);
    assert_eq!(d.profile, "expo");
    assert_eq!(d.evidence, vec!["app.json (\"expo\")"]);
    assert_eq!(d.commands, CommandsConfig::default());

    for cfg in [
        "app.config.ts",
        "app.config.js",
        "app.config.mjs",
        "app.config.cjs",
    ] {
        let by_cfg = detect(&[
            (cfg, "import 'expo/config';\nexport default {};\n"),
            ("package.json", RN_PACKAGE),
        ]);
        assert_eq!(by_cfg.profile, "expo", "{cfg}");
        assert_eq!(by_cfg.evidence, vec![cfg]);

        let non_expo = detect(&[(cfg, "export default {};\n"), ("package.json", RN_PACKAGE)]);
        assert_eq!(non_expo.profile, "react-native", "{cfg}");
    }

    let by_dep = detect(&[(
        "package.json",
        r#"{"dependencies": {"expo": "^52.0.0", "react-native": "0.76.0"}}"#,
    )]);
    assert_eq!(by_dep.profile, "expo");
    assert_eq!(by_dep.evidence, vec!["package.json (expo)"]);
}

#[test]
fn app_json_without_expo_key_is_not_expo() {
    // The word appears only inside a value, and the bare React Native template also ships app.json.
    let d = detect(&[
        ("app.json", r#"{"name": "demo", "displayName": "expo fan"}"#),
        ("package.json", RN_PACKAGE),
    ]);
    assert_eq!(d.profile, "react-native");

    let broken = detect(&[("app.json", "{ expo"), ("package.json", WEB_PACKAGE)]);
    assert_eq!(broken.profile, "web");
}

#[test]
fn react_native_by_package_dependency_and_it_wins_over_native_shells() {
    let d = detect(&[
        ("package.json", RN_PACKAGE),
        ("android/build.gradle", ""),
        ("android/app/src/main/AndroidManifest.xml", "<manifest/>"),
    ]);
    assert_eq!(d.profile, "react-native");
    assert_eq!(d.evidence, vec!["package.json (react-native)"]);
    assert_eq!(d.commands, CommandsConfig::default());

    let dev = detect(&[(
        "package.json",
        r#"{"devDependencies": {"react-native": "0.80.0"}}"#,
    )]);
    assert_eq!(dev.profile, "react-native");
}

#[test]
fn web_by_plain_package_json() {
    let d = detect(&[("package.json", WEB_PACKAGE)]);
    assert_eq!(d.profile, "web");
    assert_eq!(d.evidence, vec!["package.json"]);
    assert_eq!(d.commands.test_scoped, "npm run test");
    assert_eq!(d.commands.test_full, "npm run test");
    assert_eq!(d.commands.build, "");
    assert_eq!(d.commands.lint, "");

    let full_pkg =
        r#"{"scripts": {"build": "vite build", "test": "vitest run", "lint": "eslint ."}}"#;
    for (lock, runner) in [
        ("pnpm-lock.yaml", "pnpm"),
        ("yarn.lock", "yarn"),
        ("bun.lock", "bun run"),
        ("bun.lockb", "bun run"),
        ("package-lock.json", "npm run"),
    ] {
        let w = detect(&[("package.json", full_pkg), (lock, "")]);
        assert_eq!(w.profile, "web", "{lock}");
        assert_eq!(w.commands.build, format!("{runner} build"), "{lock}");
        assert_eq!(w.commands.test_scoped, format!("{runner} test"), "{lock}");
        assert_eq!(w.commands.test_full, format!("{runner} test"), "{lock}");
        assert_eq!(w.commands.lint, format!("{runner} lint"), "{lock}");
        assert_eq!(w.evidence, vec!["package.json"], "{lock}");
    }

    // "react-native" only in a script or a name is not a dependency.
    let name_only = detect(&[(
        "package.json",
        r#"{"name": "react-native-docs", "scripts": {"x": "react-native"}}"#,
    )]);
    assert_eq!(name_only.profile, "web");
    assert_eq!(name_only.commands, CommandsConfig::default());
}

#[test]
fn backend_by_go_mod_or_pyproject() {
    let go = detect(&[("go.mod", "module x\n")]);
    assert_eq!(go.profile, "backend");
    assert_eq!(go.evidence, vec!["go.mod"]);
    assert_eq!(go.commands.build, "go build ./...");
    assert_eq!(go.commands.test_scoped, "go test ./...");
    assert_eq!(go.commands.test_full, "go test ./...");
    assert_eq!(go.commands.lint, "go vet ./...");

    let py = detect(&[("pyproject.toml", "[project]\nname = \"x\"\n")]);
    assert_eq!(py.profile, "backend");
    assert_eq!(py.evidence, vec!["pyproject.toml"]);
    // pytest, unittest, tox, nox: the runner is not knowable from the marker.
    assert_eq!(py.commands, CommandsConfig::default());

    // A backend with a small JS toolchain is still a backend.
    let mixed = detect(&[("go.mod", "module x\n"), ("package.json", WEB_PACKAGE)]);
    assert_eq!(mixed.profile, "backend");
}

#[test]
fn unity_by_project_version_and_it_wins_over_everything() {
    let d = detect(&[
        (
            "ProjectSettings/ProjectVersion.txt",
            "m_EditorVersion: 6000.0.1f1\n",
        ),
        ("package.json", WEB_PACKAGE),
        ("Cargo.toml", ""),
    ]);
    assert_eq!(d.profile, "unity");
    // The second line says why no test command was written (no test assembly).
    assert_eq!(d.evidence[0], "ProjectSettings/ProjectVersion.txt");
    assert_eq!(d.evidence.len(), 2);
    assert_eq!(d.commands, CommandsConfig::default());
}

#[test]
fn rust_wins_over_a_js_toolchain_in_the_same_repo() {
    let d = detect(&[("Cargo.toml", ""), ("package.json", WEB_PACKAGE)]);
    assert_eq!(d.profile, "rust");
}

#[test]
fn effective_profile_resolves_auto_only() {
    let dir = repo(&[("Cargo.toml", "")]);
    let mut cfg = RepoConfig::default();
    assert_eq!(effective_profile(&cfg, dir.path()), "rust");
    cfg.project.profile = "web".to_string();
    assert_eq!(effective_profile(&cfg, dir.path()), "web");
}

fn assert_round_trip(detected: &Detected, name: &str) -> String {
    let text = render_repo_config(detected, name);
    let parsed: RepoConfig = toml::from_str(&text).unwrap();
    let mut want = RepoConfig::default();
    want.project.name = name.to_string();
    want.project.profile = detected.profile.clone();
    want.commands = detected.commands.clone();
    assert_eq!(parsed, want, "rendered:\n{text}");
    text
}

#[test]
fn rendered_config_round_trips_for_every_detected_shape() {
    let fixtures: [&[(&str, &str)]; 4] = [
        &[],
        &[("Cargo.toml", "")],
        &[
            ("build.gradle", ""),
            ("gradlew", ""),
            ("app/src/main/AndroidManifest.xml", ""),
        ],
        &[("package.json", RN_PACKAGE)],
    ];
    for files in fixtures {
        assert_round_trip(&detect(files), "demo");
    }
}

#[test]
fn rendered_config_escapes_hostile_strings() {
    let detected = Detected {
        profile: "rust".to_string(),
        commands: CommandsConfig {
            build: "echo \"hi\" \\ 'there'".to_string(),
            test_scoped: "printf 'a\nb'".to_string(),
            test_full: "x # not a comment".to_string(),
            lint: "\"\"\"".to_string(),
            lint_full: "lf".to_string(),
            repro: "sh -c 'exit \"1\"' # x".to_string(),
        },
        evidence: vec!["evil\n[gate]\nstore_compliance = \"off\"".to_string()],
    };
    let text = assert_round_trip(&detected, "we\"ird\\na\nme = \"x\"");
    // Evidence is written as a comment; a newline in it must not start a real TOML line.
    assert!(
        !text
            .lines()
            .any(|l| l.trim() == "store_compliance = \"off\"")
    );
}

#[test]
fn rendered_config_is_commented_and_names_the_evidence() {
    let text = render_repo_config(&detect(&[("Cargo.toml", "")]), "demo");
    assert!(text.starts_with('#'));
    assert!(text.contains("Cargo.toml"));
    assert!(text.contains("Unverified"));
    assert!(text.lines().any(|l| l == "repro = \"\""), "{text}");
    for section in [
        "[project]",
        "[commands]",
        "[gate]",
        "[unblocker]",
        "[run]",
        "[cross_audit]",
        "[proof]",
        "[github]",
        "[github.columns]",
    ] {
        assert!(text.lines().any(|l| l == section), "missing {section}");
    }
    assert!(text.lines().filter(|l| l.starts_with('#')).count() >= 8);
}

#[test]
fn detection_ignores_unreadable_markers() {
    // package.json is a directory: treated as "no usable marker", not a panic.
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join("package.json")).unwrap();
    let d = detect_stack(Path::new(dir.path()));
    assert_eq!(d.profile, "auto");
}

#[test]
fn android_with_product_flavors_gets_no_commands_and_says_why() {
    // With flavors the tasks are `test<Flavor>DebugUnitTest` etc.; the plain
    // `testDebugUnitTest` does not exist, so nothing is certain.
    for (gradle_file, body) in [
        (
            "app/build.gradle.kts",
            "android {\n    flavorDimensions += \"tier\"\n    productFlavors {\n        create(\"free\") {}\n    }\n}\n",
        ),
        (
            "app/build.gradle",
            "android {\n  productFlavors {\n    free {}\n    paid {}\n  }\n}\n",
        ),
        (
            "mobile/build.gradle",
            "android { productFlavors { demo {} } }\n",
        ),
    ] {
        let module = gradle_file.split('/').next().unwrap();
        let manifest = format!("{module}/src/main/AndroidManifest.xml");
        let d = detect(&[
            ("build.gradle", ""),
            ("gradlew", "#!/bin/sh\n"),
            (manifest.as_str(), "<manifest/>"),
            (gradle_file, body),
        ]);
        assert_eq!(d.profile, "android", "{gradle_file}");
        assert_eq!(d.commands, CommandsConfig::default(), "{gradle_file}");
        let why = d
            .evidence
            .iter()
            .find(|line| line.contains("productFlavors"))
            .unwrap_or_else(|| panic!("no evidence line in {:?}", d.evidence));
        assert!(why.contains(gradle_file), "{why}");
        assert!(why.contains("commands left empty"), "{why}");
        assert_round_trip(&d, "demo");
    }

    // An app module without flavors keeps the certain commands.
    let d = detect(&[
        ("build.gradle", ""),
        ("gradlew", "#!/bin/sh\n"),
        ("app/src/main/AndroidManifest.xml", "<manifest/>"),
        (
            "app/build.gradle.kts",
            "android {\n    buildTypes { release {} }\n}\n",
        ),
    ]);
    assert_eq!(
        d.commands.test_full,
        "./gradlew testDebugUnitTest --no-daemon"
    );
    assert!(
        !d.evidence
            .iter()
            .any(|line| line.contains("productFlavors"))
    );
}
