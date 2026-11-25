#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Default gate commands `aaf init` derives beyond the package scripts: no
//! watch-mode test that would hang the gate, `tsc --noEmit` as a lint for
//! TypeScript projects, and Python tools run through the project's
//! environment manager.

use std::fs;

use aaf_engine::config::{Detected, detect_stack};
use tempfile::TempDir;

fn detect(files: &[(&str, &str)]) -> Detected {
    let dir = TempDir::new().unwrap();
    for (rel, body) in files {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
    detect_stack(dir.path())
}

#[test]
fn expo_template_watch_test_is_replaced_by_a_one_shot_run() {
    // `npx create-expo-app` ships `"test": "jest --watchAll"`, which never exits.
    let pkg = r#"{"dependencies": {"expo": "^52.0.0", "react-native": "0.76.0"},
        "devDependencies": {"jest-expo": "~52.0.0"},
        "scripts": {"test": "jest --watchAll"}}"#;
    let d = detect(&[("package.json", pkg)]);
    assert_eq!(d.profile, "expo");
    assert_eq!(d.commands.test_scoped, "npx jest --ci");
    assert_eq!(d.commands.test_full, "npx jest --ci");

    for (script, want) in [
        ("jest --watch", "npx jest --ci"),
        ("jest --watchAll=true", "npx jest --ci"),
        ("vitest watch", "npx vitest run"),
    ] {
        let pkg = format!(r#"{{"scripts": {{"test": "{script}"}}}}"#);
        let d = detect(&[("package.json", &pkg)]);
        assert_eq!(d.commands.test_scoped, want, "{script}");
    }
    // Explicitly not watching, or a bare vitest (no TTY in the gate): kept.
    for script in ["jest --watchAll=false", "vitest", "vitest run", "jest --ci"] {
        let pkg = format!(r#"{{"scripts": {{"test": "{script}"}}}}"#);
        let d = detect(&[("package.json", &pkg)]);
        assert_eq!(d.commands.test_scoped, "npm run test", "{script}");
    }
    // A watcher with no known one-shot form: no test rather than a hang.
    let other = detect(&[(
        "package.json",
        r#"{"scripts": {"test": "karma start --watch"}}"#,
    )]);
    assert_eq!(other.commands.test_scoped, "");
}

#[test]
fn typescript_projects_without_a_lint_script_get_tsc_no_emit() {
    let pkg = r#"{"devDependencies": {"typescript": "^5.6.0"}, "scripts": {"test": "vitest run"}}"#;
    let d = detect(&[("package.json", pkg), ("tsconfig.json", "{}")]);
    assert_eq!(d.commands.lint, "npx tsc --noEmit");

    // No scripts at all still gets the type check.
    let bare = detect(&[
        (
            "package.json",
            r#"{"devDependencies": {"typescript": "5.6.0"}}"#,
        ),
        ("tsconfig.json", "{}"),
    ]);
    assert_eq!(bare.commands.lint, "npx tsc --noEmit");
    assert_eq!(bare.commands.test_scoped, "");

    // TypeScript not declared: npx would download it inside the gate.
    let undeclared = detect(&[("package.json", "{}"), ("tsconfig.json", "{}")]);
    assert_eq!(undeclared.commands.lint, "");
    // A lint script wins.
    let scripted = detect(&[
        (
            "package.json",
            r#"{"devDependencies": {"typescript": "5"}, "scripts": {"lint": "eslint ."}}"#,
        ),
        ("tsconfig.json", "{}"),
    ]);
    assert_eq!(scripted.commands.lint, "npm run lint");
}

#[test]
fn python_tools_run_through_the_lock_files_environment_manager() {
    let pyproject =
        "[project]\nname = \"x\"\n\n[tool.pytest.ini_options]\n\n[tool.ruff]\n\n[tool.mypy]\n";
    for (lock, prefix) in [
        ("uv.lock", "uv run "),
        ("poetry.lock", "poetry run "),
        ("pdm.lock", "pdm run "),
    ] {
        let d = detect(&[("pyproject.toml", pyproject), (lock, "")]);
        assert_eq!(d.commands.test_scoped, format!("{prefix}pytest"), "{lock}");
        assert_eq!(d.commands.lint, format!("{prefix}ruff check ."), "{lock}");
        assert_eq!(d.commands.lint_full, format!("{prefix}mypy ."), "{lock}");
    }
    let plain = detect(&[("pyproject.toml", pyproject)]);
    assert_eq!(plain.commands.test_scoped, "pytest");
    assert_eq!(plain.commands.lint_full, "mypy .");

    // Tool config in its own file counts as well.
    let files = detect(&[
        ("pyproject.toml", "[project]\nname = \"x\"\n"),
        ("ruff.toml", "line-length = 100\n"),
        ("mypy.ini", "[mypy]\n"),
    ]);
    assert_eq!(files.commands.lint, "ruff check .");
    assert_eq!(files.commands.lint_full, "mypy .");
    let dotted = detect(&[
        ("pyproject.toml", "[project]\nname = \"x\"\n"),
        (".ruff.toml", ""),
    ]);
    assert_eq!(dotted.commands.lint, "ruff check .");
}

#[test]
fn go_projects_include_gofmt_in_full_lint() {
    let d = detect(&[("go.mod", "module example.com/demo\n")]);
    assert_eq!(d.profile, "backend");
    assert_eq!(d.commands.build, "go build ./...");
    assert_eq!(d.commands.test_scoped, "go test ./...");
    assert_eq!(d.commands.lint, "go vet ./...");
    assert_eq!(d.commands.lint_full, "test -z \"$(gofmt -l .)\"");
}

#[test]
fn ios_xcodeproj_only_gets_xcodebuild_when_exactly_one_shared_scheme_exists() {
    let single = detect(&[
        ("Demo.xcodeproj/project.pbxproj", ""),
        (
            "Demo.xcodeproj/xcshareddata/xcschemes/DemoApp.xcscheme",
            "<Scheme/>",
        ),
    ]);
    assert_eq!(single.profile, "ios");
    assert_eq!(
        single.commands.build,
        "xcodebuild -project Demo.xcodeproj -scheme DemoApp -destination 'generic/platform=iOS Simulator' CODE_SIGNING_ALLOWED=NO build"
    );
    assert!(single.evidence.contains(&"DemoApp.xcscheme".to_string()));

    // Two shared schemes: ambiguous, leave commands empty.
    let multi = detect(&[
        ("Demo.xcodeproj/project.pbxproj", ""),
        (
            "Demo.xcodeproj/xcshareddata/xcschemes/DemoApp.xcscheme",
            "<Scheme/>",
        ),
        (
            "Demo.xcodeproj/xcshareddata/xcschemes/DemoAdmin.xcscheme",
            "<Scheme/>",
        ),
    ]);
    assert_eq!(multi.profile, "ios");
    assert_eq!(multi.commands.build, "");

    // Workspace falls back to sibling `.xcodeproj` shared scheme when unique.
    let ws = detect(&[
        ("Demo.xcworkspace/contents.xcworkspacedata", ""),
        ("Demo.xcodeproj/project.pbxproj", ""),
        (
            "Demo.xcodeproj/xcshareddata/xcschemes/DemoApp.xcscheme",
            "<Scheme/>",
        ),
    ]);
    assert_eq!(ws.profile, "ios");
    assert_eq!(
        ws.commands.build,
        "xcodebuild -workspace Demo.xcworkspace -scheme DemoApp -destination 'generic/platform=iOS Simulator' CODE_SIGNING_ALLOWED=NO build"
    );
}

#[test]
fn python_requirements_txt_without_pyproject_detects_pytest() {
    let with_tests = detect(&[
        ("requirements.txt", "flask==3.0.0\n"),
        ("tests/test_app.py", "def test_ok(): pass\n"),
        ("ruff.toml", "line-length = 100\n"),
        ("mypy.ini", "[mypy]\n"),
    ]);
    assert_eq!(with_tests.profile, "backend");
    assert_eq!(with_tests.evidence, vec!["requirements.txt"]);
    assert_eq!(with_tests.commands.test_scoped, "python3 -m pytest");
    assert_eq!(with_tests.commands.test_full, "python3 -m pytest");
    assert_eq!(with_tests.commands.lint, "ruff check .");
    assert_eq!(with_tests.commands.lint_full, "mypy .");

    let by_dev_dep = detect(&[
        ("requirements.txt", "requests>=2.31\n"),
        ("requirements-dev.txt", "pytest>=8.0\n"),
    ]);
    assert_eq!(by_dev_dep.profile, "backend");
    assert_eq!(by_dev_dep.commands.test_scoped, "python3 -m pytest");

    let bare = detect(&[("requirements.txt", "requests>=2.31\n")]);
    assert_eq!(bare.profile, "backend");
    assert_eq!(bare.commands.test_scoped, "");
}
