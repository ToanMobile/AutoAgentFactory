//! Oracle for G4: TEST-SKIP static rule and ALLOW-001 inline-suppression finding.
//!
//! Frozen after Phase 0. Trigger strings are assembled from fragments so this
//! test file does not trip the repository's own static gate rules.

#![allow(clippy::unwrap_used)]

use aaf_engine::gate::changes::AddedLines;
use aaf_engine::gate::static_checks::scan;

fn lines(path: &str, texts: &[&str]) -> Vec<AddedLines> {
    vec![AddedLines {
        path: path.to_string(),
        lines: texts
            .iter()
            .zip(1u32..)
            .map(|(text, number)| (number, (*text).to_string()))
            .collect(),
    }]
}

/// Case 1: adding a Rust ignore attribute in `tests/foo.rs` must emit TEST-SKIP.
#[test]
fn ignore_in_test_file_produces_test_skip_finding() {
    let ignore_attr = ["#[ig", "nore]"].concat();
    let files = lines("tests/foo.rs", &[&ignore_attr, "fn skipped_test() {}"]);
    let report = scan(&files).unwrap();
    let hit = report
        .findings
        .iter()
        .find(|f| f.rule == "TEST-SKIP")
        .unwrap_or_else(|| {
            panic!(
                "expected a TEST-SKIP finding but got: {:?}",
                report.findings
            )
        });
    assert_eq!(hit.line, Some(1));
}

/// Case 2: adding a line with an inline allow marker in `src/lib.rs` must emit ALLOW-001.
#[test]
fn newly_added_allow_marker_produces_allow_001_finding() {
    let line = ["to", "do!() // aaf:al", "low(LAZY-001) later"].concat();
    let files = lines("src/lib.rs", &[&line]);
    let report = scan(&files).unwrap();
    let hit = report
        .findings
        .iter()
        .find(|f| f.rule == "ALLOW-001")
        .unwrap_or_else(|| {
            panic!(
                "expected an ALLOW-001 finding but got: {:?}",
                report.findings
            )
        });
    assert_eq!(hit.line, Some(1));
}
