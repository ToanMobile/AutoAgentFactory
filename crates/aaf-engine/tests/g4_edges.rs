//! Extra edge-case tests for G4: TEST-SKIP static checks and ALLOW-001 head-lock.
//! Trigger strings are fragmented so this file never trips static gate rules.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use aaf_domain::config::RepoConfig;
use aaf_domain::gate::{LockStatus, Severity};
use aaf_domain::ports::Clock;
use aaf_engine::gate::changes::AddedLines;
use aaf_engine::gate::{head_lock, static_checks};
use tempfile::TempDir;

fn j(parts: &[&str]) -> String {
    parts.concat()
}

fn scan_lines(path: &str, texts: &[&str]) -> static_checks::StaticReport {
    static_checks::scan(&[AddedLines {
        path: path.to_string(),
        lines: texts
            .iter()
            .zip(1u32..)
            .map(|(t, n)| (n, (*t).to_string()))
            .collect(),
    }])
    .unwrap()
}

fn rules(path: &str, line: &str) -> Vec<String> {
    scan_lines(path, &[line])
        .findings
        .into_iter()
        .map(|f| f.rule)
        .collect()
}

#[test]
fn test_skip_scoping_and_multi_language_patterns() {
    // Rust Iterator::skip is never flagged; inline #[cfg(test)] ignore IS flagged as Warn.
    assert!(rules("src/lib.rs", "let v: Vec<_> = iter.skip(1).collect();").is_empty());
    let rs_attr = j(&["    #[ig", "nore]"]);
    let rs = scan_lines("src/foo.rs", &["#[cfg(test)]", "mod tests {", &rs_attr]);
    assert_eq!(rs.findings.len(), 1);
    assert_eq!(rs.findings[0].rule, "TEST-SKIP");
    assert_eq!(rs.findings[0].severity, Severity::Warn);
    assert_eq!(rs.findings[0].line, Some(3));

    // Kotlin production model stays clean; test files across languages trigger TEST-SKIP.
    let kt_ann = j(&["@Ig", "nore"]);
    assert!(rules("app/src/main/kotlin/Model.kt", &kt_ann).is_empty());
    let ts_skip = j(&["test.sk", "ip('flaky', () => {});"]);
    assert!(rules("src/helpers.ts", &ts_skip).is_empty());
    for (path, line) in [
        ("src/foo.rs", j(&["#[test] #[ig", "nore]"])),
        ("src/foo.rs", j(&["#[test, ig", "nore]"])),
        ("src/foo.rs", j(&["#[cfg_attr(unix, ig", "nore)]"])),
        ("app/src/test/kotlin/FooTest.kt", kt_ann),
        ("app/TestOrder.kt", j(&["@Test @Ig", "nore"])),
        ("src/test/java/FooTest.java", j(&["@Dis", "abled"])),
        ("src/TestOrder.java", j(&["@Test @Dis", "abled"])),
        ("src/TestOrder.java", j(&["@Dis", "abledIf(\"isCi\")"])),
        ("src/foo.test.ts", ts_skip),
        ("src/foo.tests.ts", j(&["it.sk", "ip.each([1])('a', fn)"])),
        ("tests/app.spec.js", j(&["xd", "escribe('s', () => {});"])),
        ("tests/app.spec.js", j(&["await x", "it('s', () => {});"])),
        (
            "tests/app.spec.js",
            j(&["describe.sk", "ip.each([1])('s')"]),
        ),
        ("tests/app.spec.js", j(&["it.concurrent.sk", "ip('s', fn)"])),
        ("tests/app.spec.js", j(&["it.to", "do('later')"])),
        ("tests/app.spec.js", j(&["test.to", "do('later')"])),
        (
            "tests/test_foo.py",
            j(&["@pytest.mark.sk", "ip(reason='x')"]),
        ),
        (
            "unit_tests/a.py",
            j(&["pytestmark = pytest.mark.sk", "ip()"]),
        ),
        ("unit_tests/a.py", j(&["raise unittest.Sk", "ipTest('x')"])),
        ("pkg/foo_test.go", j(&["t.Sk", "ip(\"wip\")"])),
        (
            "test/widget_test.dart",
            j(&["test('x', () {}, sk", "ip: true);"]),
        ),
        ("FooTest.cs", j(&["[Ig", "nore]"])),
        ("FooTest.cs", j(&["[Test, Ig", "nore(\"wip\")]"])),
        ("FooTest.cs", j(&["[Test] [Ig", "nore]"])),
        ("FooTest.cs", j(&["[Fact(Sk", "ip = \"wip\")]"])),
        ("FooTests.swift", j(&["throw XCTSk", "ip(\"wip\")"])),
        ("FooTests.swift", j(&["try XCTSk", "ipIf(cond)"])),
        ("FooTests.swift", j(&["try XCTSk", "ipUnless(cond)"])),
    ] {
        assert_eq!(rules(path, &line), vec!["TEST-SKIP"], "{path}: {line}");
    }
    for path in [
        "src/tests.rs",
        "src/test.rs",
        "pkg/foo_tests.rs",
        "web/a.tests.ts",
        "app/TestOrder.java",
        "app/TestOrder.kt",
        "unit_tests/a.py",
        "app/integrationTest/A.kt",
    ] {
        assert!(static_checks::is_test_path(path), "{path}");
    }
}

#[test]
fn allow_marker_supports_test_skip_and_head_lock_requires_approval() {
    let allow_skip = j(&["#[ig", "nore] // aaf:al", "low(TEST-SKIP) upstream bug"]);
    assert!(static_checks::contains_allow_marker(&allow_skip).unwrap());
    assert!(!static_checks::contains_allow_marker("let x = 1;").unwrap());
    let report = scan_lines("tests/foo.rs", &[&allow_skip]);
    assert_eq!(report.suppressions, 1);
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].rule, "ALLOW-001");
    assert_eq!(report.findings[0].severity, Severity::Warn);

    struct At(i64);
    impl Clock for At {
        fn now(&self) -> i64 {
            self.0
        }
    }
    let git = |repo: &Path, args: &[&str]| {
        let s = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .status()
            .unwrap();
        assert!(s.success(), "git {args:?}");
    };
    let (repo, data) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let root = repo.path();
    git(root, &["init", "-q", "-b", "main"]);
    fs::create_dir_all(root.join("src")).unwrap();
    let old = j(&["pub fn old() {} // aaf:al", "low(LOG-001) legacy\n"]);
    fs::write(root.join("src/existing.rs"), &old).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn hello() {}\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "base"]);

    let mut cfg = RepoConfig::default();
    cfg.commands.test_full = "true".to_string();
    let base = aaf_fp::git(root, &["rev-parse", "HEAD"], &[], Duration::from_secs(10)).unwrap();

    // Unrelated edit in a file with a pre-existing allow marker does NOT lock.
    fs::write(
        root.join("src/existing.rs"),
        format!("{old}pub fn b() {{}}\n"),
    )
    .unwrap();
    assert_eq!(
        head_lock::evaluate(data.path(), root, &cfg, &base).status,
        LockStatus::Clear
    );

    // Newly added allow marker blocks head-lock until approved, then clears.
    let added = j(&["pub fn hello() {} // aaf:al", "low(LAZY-001) temp\n"]);
    fs::write(root.join("src/lib.rs"), &added).unwrap();
    let blocked = head_lock::evaluate(data.path(), root, &cfg, &base);
    assert_eq!(blocked.status, LockStatus::Blocked, "{blocked:?}");
    head_lock::approve(data.path(), root, &cfg, "rev", false, &At(100)).unwrap();
    assert_eq!(
        head_lock::evaluate(data.path(), root, &cfg, &base).status,
        LockStatus::Approved
    );
}
