#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Stack detection on layouts cut down from real repositories: a Unity game
//! with Unity Test Framework assemblies, a Unity project without tests, and
//! a monorepo whose Gradle builds live one directory down.

use std::fs;
use std::path::Path;
use std::process::Command;

use aaf_domain::config::CommandsConfig;
use aaf_engine::config::{Detected, detect_stack};
use aaf_engine::throttle::{HeavyTool, heavy_tool};
use tempfile::TempDir;

#[path = "../../aaf-fp/tests/exec_support.rs"]
mod exec_support;
use exec_support::write_executable;

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

const VERSION: &str =
    "m_EditorVersion: 6000.6.0f1\nm_EditorVersionWithRevision: 6000.6.0f1 (f7f8ed4d1e24)\n";
const VERSION_FILE: &str = "ProjectSettings/ProjectVersion.txt";

const EDITMODE_ASMDEF: &str = r#"{
    "name": "Idle.Tests.EditMode",
    "references": ["Idle.Sim", "UnityEngine.TestRunner", "UnityEditor.TestRunner"],
    "includePlatforms": ["Editor"],
    "overrideReferences": true,
    "precompiledReferences": ["nunit.framework.dll"],
    "defineConstraints": ["UNITY_INCLUDE_TESTS"]
}"#;
const PLAYMODE_ASMDEF: &str = r#"{
    "name": "Idle.Tests.PlayMode",
    "references": ["Idle.Runtime", "UnityEngine.TestRunner"],
    "includePlatforms": [],
    "precompiledReferences": ["nunit.framework.dll"],
    "defineConstraints": ["UNITY_INCLUDE_TESTS"]
}"#;
const RUNTIME_ASMDEF: &str = r#"{"name": "Idle.Runtime", "references": []}"#;
const EDITOR_ASMDEF: &str =
    r#"{"name": "Idle.Editor", "references": ["Idle.Runtime"], "includePlatforms": ["Editor"]}"#;

const EDITMODE_REL: &str = "Assets/_Project/Tests/EditMode/Idle.Tests.EditMode.asmdef";

fn unity_with_tests() -> Vec<(&'static str, &'static str)> {
    vec![
        (VERSION_FILE, VERSION),
        (
            "Assets/_Project/Scripts/Runtime/Idle.Runtime.asmdef",
            RUNTIME_ASMDEF,
        ),
        (
            "Assets/_Project/Scripts/Editor/Idle.Editor.asmdef",
            EDITOR_ASMDEF,
        ),
        (EDITMODE_REL, EDITMODE_ASMDEF),
        (
            "Assets/_Project/Tests/PlayMode/Idle.Tests.PlayMode.asmdef",
            PLAYMODE_ASMDEF,
        ),
    ]
}

#[test]
fn unity_with_an_editmode_test_assembly_gets_a_batchmode_test_command() {
    let d = detect(&unity_with_tests());
    assert_eq!(d.profile, "unity");
    let test = &d.commands.test_full;
    assert_eq!(&d.commands.test_scoped, test);
    // The editor of the pinned version, at the Unity Hub location, unless
    // the machine says otherwise.
    assert!(
        test.contains(
            "\"${UNITY_PATH:-/Applications/Unity/Hub/Editor/6000.6.0f1/Unity.app/Contents/MacOS/Unity}\""
        ),
        "{test}"
    );
    for part in [
        "-batchmode",
        "-runTests",
        "-testPlatform EditMode",
        "-testResults",
    ] {
        assert!(test.contains(part), "{part}: {test}");
    }
    // `-runTests` ends the editor itself; `-quit` would end it before the tests.
    assert!(!test.contains("-quit"), "{test}");
    // A player build or a lint is not certain for a Unity project.
    assert_eq!(d.commands.build, "");
    assert_eq!(d.commands.lint, "");
    assert_eq!(d.evidence, vec![VERSION_FILE, EDITMODE_REL]);
    // The throttle must see it as a Unity run: it runs alone.
    assert_eq!(heavy_tool(test), Some(HeavyTool::Unity));
}

#[test]
fn unity_project_without_an_editmode_test_assembly_has_nothing_to_verify() {
    // Runtime, editor tooling and PlayMode tests only: an EditMode run would
    // execute zero tests, which is neither a pass nor a failure of the code.
    let d = detect(&[
        (VERSION_FILE, VERSION),
        (
            "Assets/_Project/Scripts/Runtime/Idle.Runtime.asmdef",
            RUNTIME_ASMDEF,
        ),
        (
            "Assets/_Project/Scripts/Editor/Idle.Editor.asmdef",
            EDITOR_ASMDEF,
        ),
        (
            "Assets/_Project/Tests/PlayMode/Idle.Tests.PlayMode.asmdef",
            PLAYMODE_ASMDEF,
        ),
    ]);
    assert_eq!(d.profile, "unity");
    assert_eq!(d.commands, CommandsConfig::default());
    assert_eq!(d.evidence.len(), 2, "{:?}", d.evidence);
    assert!(
        d.evidence[1].contains("no EditMode test assembly"),
        "{:?}",
        d.evidence
    );
}

#[test]
fn docs_only_repo_with_an_assets_folder_is_not_a_unity_project() {
    let d = detect(&[
        ("README.md", "# game design\n"),
        ("Assets/readme.txt", "art goes here\n"),
    ]);
    assert_eq!(d.profile, "auto");
    assert_eq!(d.commands, CommandsConfig::default());
}

#[test]
fn editor_version_that_is_not_a_plain_version_never_reaches_the_shell() {
    for version in [
        "m_EditorVersion: 6000.6.0f1; touch pwned\n",
        "m_EditorVersion: $(touch pwned)\n",
        "m_EditorVersion: ../../../tmp/x\n",
        "m_EditorVersion: \n",
        "nothing here\n",
    ] {
        let d = detect(&[(VERSION_FILE, version), (EDITMODE_REL, EDITMODE_ASMDEF)]);
        assert_eq!(d.profile, "unity", "{version:?}");
        assert_eq!(d.commands, CommandsConfig::default(), "{version:?}");
        assert!(
            d.evidence.iter().any(|e| e.contains("m_EditorVersion")),
            "{version:?}: {:?}",
            d.evidence
        );
    }
}

/// A stand-in for the editor: takes the path after `-testResults` and writes
/// `results` there (nothing when empty), then logs `noise` bytes of shutdown
/// lines, as the real editor does after the test run, and exits with `code`.
fn fake_editor(dir: &Path, results: &str, code: i32, noise: usize) -> String {
    let path = dir.join("fake-unity");
    let write = if results.is_empty() {
        String::new()
    } else {
        format!("printf '%s' '{results}' > \"$out\"\n")
    };
    let line = "Thread 0x170573000 may have been prematurely finalized\n";
    let shutdown = line.repeat(noise.div_ceil(line.len()));
    let script = format!(
        "#!/bin/sh\nwhile [ $# -gt 0 ]; do\n  if [ \"$1\" = -testResults ]; then out=\"$2\"; fi\n  shift\ndone\n{write}echo fake editor ran\nprintf '%s' '{shutdown}'\nexit {code}\n"
    );
    write_executable(&path, script);
    path.to_string_lossy().into_owned()
}

struct Generated {
    code: Option<i32>,
    /// Whatever the run left in the project besides the fixture itself.
    left: Vec<String>,
    /// The end of the output, as much as the gate keeps in the receipt.
    tail: String,
}

/// Bytes of output per command the gate keeps (`gate::runner::LOG_TAIL_BYTES`).
const KEPT: usize = 4096;

/// Runs the generated command the way the gate does, with the fake editor.
fn run_with_noise(results: &str, code: i32, noise: usize) -> Generated {
    let dir = repo(&unity_with_tests());
    let command = detect_stack(dir.path()).commands.test_full;
    let editor = fake_editor(dir.path(), results, code, noise);
    let done = Command::new("sh")
        .arg("-c")
        .arg(&command)
        .current_dir(dir.path())
        .env("UNITY_PATH", editor)
        .output()
        .unwrap();
    let mut left: Vec<String> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| !["Assets", "ProjectSettings", "fake-unity"].contains(&name.as_str()))
        .collect();
    left.sort();
    let mut output = done.stdout;
    output.extend(done.stderr);
    let kept = output.split_off(output.len().saturating_sub(KEPT));
    Generated {
        code: done.status.code(),
        left,
        tail: String::from_utf8_lossy(&kept).into_owned(),
    }
}

fn run_generated(results: &str, code: i32) -> (Option<i32>, Vec<String>) {
    let run = run_with_noise(results, code, 0);
    (run.code, run.left)
}

const PASSED: &str =
    r#"<test-run total="1" passed="1" failed="0"><test-case name="A" result="Passed"/></test-run>"#;
const FAILED: &str =
    r#"<test-run total="1" passed="0" failed="1"><test-case name="A" result="Failed"/></test-run>"#;

#[test]
fn generated_unity_command_passes_only_when_tests_ran_and_none_failed() {
    // Tests ran and passed; nothing is written into the project.
    assert_eq!(run_generated(PASSED, 0), (Some(0), Vec::new()));
    // The editor's own verdict stands.
    assert_eq!(run_generated(FAILED, 2).0, Some(2));
    // Exit 0 without a single test case (compile error, filter matched
    // nothing): not a pass.
    assert_eq!(run_generated("", 0).0, Some(1));
    assert_eq!(run_generated("<test-run total=\"0\"/>", 0).0, Some(1));
    // Exit 0 with a failed case in the results: not a pass either.
    assert_eq!(run_generated(FAILED, 0).0, Some(1));
}

/// A results file in the NUnit 3 format the Unity Test Framework writes: one
/// element per line, suites carrying their own `result`. Written by hand
/// (the command deletes the real file); the count line was checked against
/// the real editor, 13 cases for the 13 tests of the project.
const NUNIT_RESULTS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<test-run id="2" testcasecount="3" result="Failed(Child)" total="3" passed="2" failed="1" inconclusive="0" skipped="0" asserts="0" engine-version="3.5.0.0">
  <test-suite type="Assembly" id="1010" name="Idle.Tests.EditMode.dll" fullname="/p/Library/ScriptAssemblies/Idle.Tests.EditMode.dll" runstate="Runnable" testcasecount="3" result="Failed" site="Child" total="3" passed="2" failed="1">
    <test-case id="1001" name="Earns" fullname="Idle.Tests.EconomyTests.Earns" methodname="Earns" classname="Idle.Tests.EconomyTests" runstate="Runnable" seed="1" result="Passed" duration="0.01" asserts="0">
      <output><![CDATA[]]></output>
    </test-case>
    <test-case id="1002" name="Saves" fullname="Idle.Tests.SaveTests.Saves" methodname="Saves" classname="Idle.Tests.SaveTests" runstate="Runnable" seed="2" result="Failed" duration="0.02" asserts="0">
      <failure><message><![CDATA[  Expected: 2  But was: 1]]></message></failure>
    </test-case>
    <test-case id="1003" name="Loads" fullname="Idle.Tests.SaveTests.Loads" methodname="Loads" classname="Idle.Tests.SaveTests" runstate="Runnable" seed="3" result="Passed" duration="0.01" asserts="0" />
  </test-suite>
</test-run>"#;

#[test]
fn generated_unity_command_ends_its_output_with_the_test_counts_and_failed_names() {
    // Seen on the real editor (6000.6.0f1): after the test run it logs more
    // shutdown lines than the gate keeps, so the kept tail of a failed run
    // named no test and no count. The command's own last lines must.
    let failed = run_with_noise(NUNIT_RESULTS, 2, 3 * KEPT);
    assert_eq!(failed.code, Some(2));
    assert!(
        failed.tail.ends_with(
            "aaf: failed: Idle.Tests.SaveTests.Saves\naaf: EditMode test cases: 3, failed: 1\n"
        ),
        "{}",
        failed.tail
    );
    assert!(!failed.tail.contains("EconomyTests"), "{}", failed.tail);

    let passed = run_with_noise(&NUNIT_RESULTS.replace("Failed", "Passed"), 0, 3 * KEPT);
    assert_eq!(passed.code, Some(0));
    assert!(
        passed
            .tail
            .ends_with("aaf: EditMode test cases: 3, failed: 0\n"),
        "{}",
        passed.tail
    );
    assert!(!passed.tail.contains("aaf: failed:"), "{}", passed.tail);
    assert_eq!(passed.left, Vec::<String>::new());

    // Results written in one line: still counted per case.
    let one_line = run_with_noise(
        r#"<test-run><test-case fullname="A.B" result="Failed"/><test-case fullname="A.C" result="Passed"/></test-run>"#,
        0,
        0,
    );
    assert_eq!(one_line.code, Some(1));
    assert!(
        one_line
            .tail
            .ends_with("aaf: failed: A.B\naaf: EditMode test cases: 2, failed: 1\n"),
        "{}",
        one_line.tail
    );
}

const SETTINGS: &str = "rootProject.name = \"x\"\ninclude(\":app\")\n";

#[test]
fn gradle_builds_one_directory_down_are_found_as_android() {
    // No build file at the root; each product has its own Gradle build.
    let d = detect(&[
        ("CarConnect/settings.gradle.kts", SETTINGS),
        ("CarConnect/gradlew", "#!/bin/sh\n"),
        (
            "CarConnect/app/build.gradle.kts",
            "android {\n    productFlavors {\n        create(\"system\") {}\n    }\n}\n",
        ),
        ("CarConnect/app/src/main/AndroidManifest.xml", "<manifest/>"),
        ("PhoneConnect/settings.gradle.kts", SETTINGS),
        ("PhoneConnect/gradlew", "#!/bin/sh\n"),
        (
            "PhoneConnect/app/src/main/AndroidManifest.xml",
            "<manifest/>",
        ),
        ("PCConnect/go.mod", "module pc\n"),
        ("docs/readme.md", "x"),
    ]);
    assert_eq!(d.profile, "android");
    // Which variant tasks to run in which build is the repo's own decision.
    assert_eq!(d.commands, CommandsConfig::default());
    assert_eq!(
        d.evidence[..2],
        [
            "CarConnect/settings.gradle.kts",
            "PhoneConnect/settings.gradle.kts"
        ]
    );
    assert!(
        d.evidence[2].contains("2 Gradle builds below the root"),
        "{:?}",
        d.evidence
    );
}

#[test]
fn single_gradle_build_one_directory_down_gets_commands_that_enter_it() {
    let d = detect(&[
        ("android-app/settings.gradle", "include ':app'\n"),
        ("android-app/gradlew", "#!/bin/sh\n"),
        (
            "android-app/app/src/main/AndroidManifest.xml",
            "<manifest/>",
        ),
    ]);
    assert_eq!(d.profile, "android");
    assert_eq!(
        d.commands.test_full,
        "cd android-app && ./gradlew testDebugUnitTest --no-daemon"
    );
    assert_eq!(
        d.commands.build,
        "cd android-app && ./gradlew assembleDebug --no-daemon"
    );
    assert_eq!(d.commands.lint, "");
    assert_eq!(
        d.commands.lint_full,
        "cd android-app && ./gradlew lint --no-daemon"
    );
    assert_eq!(heavy_tool(&d.commands.lint_full), Some(HeavyTool::Gradle));
    // A directory name the shell would read as more than a name: no command.
    let odd = detect(&[
        ("my app/settings.gradle", "include ':app'\n"),
        ("my app/gradlew", "#!/bin/sh\n"),
        ("my app/app/src/main/AndroidManifest.xml", "<manifest/>"),
    ]);
    assert_eq!(odd.profile, "android");
    assert_eq!(odd.commands, CommandsConfig::default());
}

#[test]
fn root_marker_wins_over_a_gradle_build_one_directory_down() {
    let d = detect(&[
        ("Cargo.toml", "[workspace]\n"),
        ("mobile/settings.gradle", ""),
        ("mobile/app/src/main/AndroidManifest.xml", "<manifest/>"),
    ]);
    assert_eq!(d.profile, "rust");
    // What CI of a Rust workspace runs: every member, test targets included.
    assert_eq!(
        d.commands.lint,
        "cargo clippy --workspace --all-targets -- -D warnings"
    );
}
