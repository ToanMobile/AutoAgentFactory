use std::fs;
use std::path::{Path, PathBuf};

use aaf_domain::gate::Severity;

use super::super::changes::{AddedLines, parse_added};
use super::super::static_checks::{StaticReport, is_test_path, scan, skip_path};
use super::support::fake_secret;

fn scan_text(path: &str, lines: &[&str]) -> StaticReport {
    let lines = lines
        .iter()
        .zip(1u32..)
        .map(|(text, number)| (number, (*text).to_string()))
        .collect();
    scan(&[AddedLines {
        path: path.to_string(),
        lines,
    }])
    .unwrap()
}

fn rules_hit(path: &str, line: &str) -> Vec<String> {
    scan_text(path, &[line])
        .findings
        .into_iter()
        .map(|f| f.rule)
        .collect()
}

fn j(parts: &[&str]) -> String {
    parts.concat()
}

#[test]
fn each_secret_shape_is_critical() {
    let filler = "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8s9T0";
    let secrets = [
        j(&["-----BEGIN RSA PRIVATE", " KEY-----"]),
        j(&["-----BEGIN PRIVATE", " KEY-----"]),
        fake_secret(),
        j(&["gh", "p_", &filler[..36]]),
        j(&["github", "_pat_", &filler[..30]]),
        j(&["sk", "-ant-", &filler[..24]]),
        j(&["xox", "b-", "123456789012-abcdef"]),
        j(&["AI", "za", &filler[..35]]),
    ];
    for secret in secrets {
        let report = scan_text("src/config.rs", &[&format!("let k = \"{secret}\";")]);
        let hit = report.findings.iter().find(|f| f.rule == "SEC-001");
        let hit = hit.unwrap_or_else(|| panic!("not flagged: {}", &secret[..6]));
        assert_eq!(hit.severity, Severity::Critical);
        assert_eq!(hit.line, Some(1));
        // The receipt must not repeat the secret.
        assert!(!hit.message.contains(&secret));
    }
}

#[test]
fn ordinary_lines_are_clean() {
    let clean = [
        "let key_id = lookup(\"AKIA\");",
        "let token = std::env::var(\"API_TOKEN\")?;",
        "// see the existing code below",
        "} catch (e) { log.error(e); }",
        "except ValueError: raise",
        "logger.info(\"started\");",
        "password = read_password()",
    ];
    for line in clean {
        assert_eq!(
            rules_hit("src/app.py", line),
            Vec::<String>::new(),
            "{line}"
        );
    }
}

#[test]
fn literal_credential_assignment_warns() {
    let line = j(&["db_pass", "word = \"hunter2hunter2\""]);
    let report = scan_text("src/db.py", &[&line]);
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].rule, "SEC-002");
    assert_eq!(report.findings[0].severity, Severity::Warn);
    assert_eq!(report.critical(), 0);
}

#[test]
fn placeholders_are_critical() {
    let lines = [
        j(&["// ... existing", " code ..."]),
        j(&["    to", "do!()"]),
        j(&["    unimpl", "emented!(\"later\")"]),
        j(&["# TODO: impl", "ement the retry"]),
        j(&["raise NotImplemented", "Error"]),
    ];
    for line in lines {
        assert_eq!(rules_hit("src/x.txt", &line), vec!["LAZY-001"], "{line}");
    }
}

#[test]
fn swallowed_errors_are_critical_on_one_or_two_lines() {
    let one_line = [
        j(&["} catch (e) {", " }"]),
        j(&["} catch (e: Exception) {", "}"]),
        j(&["do { try f() } catch {", "}"]),
        j(&["except Exception:", " pass"]),
        j(&["    except:", " pass"]),
    ];
    for line in one_line {
        assert_eq!(rules_hit("src/x.kt", &line), vec!["ERR-001"], "{line}");
    }
    let report = scan_text(
        "src/a.py",
        &["try:", "    run()", "except OSError:", "    pass"],
    );
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].line, Some(3));
    let report = scan_text("src/a.js", &["} catch (err) {", "}"]);
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].rule, "ERR-001");
    // A handler with a body is fine.
    let report = scan_text("src/a.js", &["} catch (err) {", "  report(err);", "}"]);
    assert!(report.findings.is_empty());
}

#[test]
fn two_line_form_needs_adjacent_added_lines() {
    let file = AddedLines {
        path: "src/a.py".to_string(),
        lines: vec![
            (3, "except OSError:".to_string()),
            (9, "    pass".to_string()),
        ],
    };
    assert!(scan(&[file]).unwrap().findings.is_empty());
}

#[test]
fn raw_logging_warns_outside_tests_only() {
    let lines = [
        j(&["console.", "log(value)"]),
        j(&["System.out.", "println(value);"]),
        j(&["e.printStack", "Trace();"]),
        j(&["let v = db", "g!(value);"]),
    ];
    for line in lines {
        assert_eq!(
            rules_hit("src/app/main.x", &line),
            vec!["LOG-001"],
            "{line}"
        );
        assert!(rules_hit("src/app/main.test.x", &line).is_empty(), "{line}");
        assert!(rules_hit("app/tests/main.x", &line).is_empty(), "{line}");
    }
    let report = scan_text("src/a.js", &[&j(&["console.", "log(1)"])]);
    assert_eq!(report.findings[0].severity, Severity::Warn);
}

#[test]
fn test_files_still_get_the_critical_rules() {
    let line = format!("const K = \"{}\";", fake_secret());
    assert_eq!(rules_hit("tests/fixtures.rs", &line), vec!["SEC-001"]);
}

#[test]
fn inline_allow_silences_only_the_named_rule_and_is_counted() {
    let marker = |rule: &str, reason: &str| j(&["// aaf:al", "low(", rule, ")", reason]);
    let stub = j(&["to", "do!()"]);
    let scan_with = |tail: String| scan_text("src/a.rs", &[&format!("{stub} {tail}")]);

    let report = scan_with(marker("LAZY-001", " fixture"));
    assert!(report.findings.is_empty());
    assert_eq!(report.suppressions, 1);

    // Wrong rule id: not silenced.
    let report = scan_with(marker("LOG-001", " fixture"));
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.suppressions, 0);

    // No reason given: not silenced.
    let report = scan_with(marker("LAZY-001", ""));
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.suppressions, 0);

    // A marker on a clean line counts nothing.
    let clean = format!("let x = 1; {}", marker("LAZY-001", " why"));
    assert_eq!(scan_text("src/a.rs", &[&clean]).suppressions, 0);
}

#[test]
fn lockfiles_and_generated_directories_are_skipped() {
    let line = format!("key = \"{}\"", fake_secret());
    for path in [
        "Cargo.lock",
        "web/package-lock.json",
        "ios/Podfile.lock",
        "target/debug/out.txt",
        "app/build/gen.kt",
        "web/node_modules/x/index.js",
        "dist/app.js",
        ".gradle/cache.txt",
        "ios/Pods/Lib/a.m",
    ] {
        assert!(skip_path(path), "{path}");
        assert!(rules_hit(path, &line).is_empty(), "{path}");
    }
    for path in ["src/target.rs", "src/builder/a.rs", "docs/lockfile.md"] {
        assert!(!skip_path(path), "{path}");
        assert_eq!(rules_hit(path, &line), vec!["SEC-001"], "{path}");
    }
}

#[test]
fn test_path_detection() {
    for path in [
        "tests/a.rs",
        "app/src/androidTest/A.kt",
        "web/__tests__/a.js",
        "pkg/foo_test.go",
        "web/a.spec.ts",
        "py/test_x.py",
        "app/FooTest.kt",
        "ios/FooTests.swift",
    ] {
        assert!(is_test_path(path), "{path}");
    }
    for path in ["src/main.rs", "src/contest.rs", "src/latest/a.js"] {
        assert!(!is_test_path(path), "{path}");
    }
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// The gate will run on this repository: its own rule definitions and tests
/// must not be findings. Paths are presented as non-test production files so
/// that no rule is skipped.
#[test]
fn the_gate_sources_do_not_trip_their_own_rules() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&crate_dir.join("src/gate"), &mut files);
    for entry in fs::read_dir(crate_dir.join("tests")).into_iter().flatten() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name.starts_with("gate_") && name.ends_with(".rs") {
            files.push(path);
        }
    }
    assert!(files.iter().any(|f| f.ends_with("static_checks.rs")));
    assert!(files.len() >= 10, "{files:?}");
    for (index, file) in files.iter().enumerate() {
        let text = fs::read_to_string(file).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let report = scan_text(&format!("src/file{index}.rs"), &lines);
        assert!(
            report.findings.is_empty() && report.suppressions == 0,
            "{}: {:?}",
            file.display(),
            report.findings
        );
    }
}

#[test]
fn diff_parser_reports_only_added_lines_with_their_numbers() {
    let diff = [
        "diff --git a/src/a.rs b/src/a.rs",
        "index 111..222 100644",
        "--- a/src/a.rs",
        "+++ b/src/a.rs",
        "@@ -3 +3,2 @@ fn x()",
        "-old",
        "+new one",
        "+new two",
        "@@ -10,0 +12 @@",
        "+tail",
        "\\ No newline at end of file",
        "diff --git a/gone.rs b/gone.rs",
        "--- a/gone.rs",
        "+++ /dev/null",
        "@@ -1,2 +0,0 @@",
        "-a",
        "-b",
        "diff --git \"a/q\\\"uote.rs\" \"b/q\\\"uote.rs\"",
        "--- \"a/q\\\"uote.rs\"",
        "+++ \"b/q\\\"uote.rs\"",
        "@@ -0,0 +1 @@",
        "+quoted",
        "diff --git a/sp ace.rs b/sp ace.rs",
        "--- a/sp ace.rs\t",
        "+++ b/sp ace.rs\t",
        "@@ -0,0 +1 @@",
        "+spaced",
    ]
    .join("\n");
    let files = parse_added(&diff);
    assert_eq!(files.len(), 3);
    assert_eq!(files[0].path, "src/a.rs");
    assert_eq!(
        files[0].lines,
        vec![
            (3, "new one".to_string()),
            (4, "new two".to_string()),
            (12, "tail".to_string())
        ]
    );
    assert_eq!(files[1].path, "q\"uote.rs");
    assert_eq!(files[1].lines, vec![(1, "quoted".to_string())]);
    assert_eq!(files[2].path, "sp ace.rs");
}

#[test]
fn diff_parser_is_not_fooled_by_content_that_looks_like_a_header() {
    // The first added line renders as a destination header for another file.
    let diff = [
        "diff --git a/a.txt b/a.txt",
        "--- a/a.txt",
        "+++ b/a.txt",
        "@@ -0,0 +1,2 @@",
        "+++ b/other.txt",
        "+second",
    ]
    .join("\n");
    let files = parse_added(&diff);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "a.txt");
    assert_eq!(files[0].lines[0], (1, "++ b/other.txt".to_string()));
    assert_eq!(files[0].lines[1], (2, "second".to_string()));
}
