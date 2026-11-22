#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Android rules of the store scanner against the fixtures in
//! `tests/fixtures/store`. Every Critical rule has a true positive, its
//! closest true negative and the cases that must stay unresolved.

use std::path::PathBuf;

use aaf_domain::config::RepoConfig;
use aaf_domain::gate::{Severity, StoreComplianceReport, StoreStatus};
use aaf_engine::store_compliance::scan;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/store")
        .join(name)
}

fn run(name: &str) -> StoreComplianceReport {
    let root = fixture(name);
    assert!(root.is_dir(), "missing fixture {name}");
    scan(&root, &RepoConfig::default(), true)
}

/// `(rule, file, line)` of every Critical, sorted.
fn criticals(report: &StoreComplianceReport) -> Vec<(String, String, Option<u32>)> {
    let mut found: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.severity == Severity::Critical)
        .map(|f| (f.rule.clone(), f.file.clone(), f.line))
        .collect();
    found.sort();
    found
}

fn crit(rule: &str, file: &str, line: u32) -> (String, String, Option<u32>) {
    (rule.to_string(), file.to_string(), Some(line))
}

fn warn_rules(report: &StoreComplianceReport) -> Vec<String> {
    let mut rules: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.severity == Severity::Warn)
        .map(|f| f.rule.clone())
        .collect();
    rules.sort();
    rules
}

/// No finding, nothing unresolved, status `Clear`.
fn assert_clean(name: &str) {
    let report = run(name);
    assert_eq!(report.status, StoreStatus::Clear, "{name}: {report:#?}");
    assert!(report.findings.is_empty(), "{name}: {:#?}", report.findings);
    assert!(
        report.unresolved.is_empty(),
        "{name}: {:#?}",
        report.unresolved
    );
}

/// Not blocked, and the given rule is listed as unresolved (with a warning).
fn assert_unresolved(name: &str, rule: &str, times: usize) -> StoreComplianceReport {
    let report = run(name);
    assert_eq!(criticals(&report), vec![], "{name}");
    assert_eq!(report.status, StoreStatus::Clear, "{name}");
    let listed = report
        .unresolved
        .iter()
        .filter(|u| u.starts_with(rule))
        .count();
    assert_eq!(listed, times, "{name}: {:#?}", report.unresolved);
    let warned = warn_rules(&report).iter().filter(|r| *r == rule).count();
    assert_eq!(
        warned, times,
        "{name}: every unresolved entry is also a warning"
    );
    report
}

// --- AND-001 ----------------------------------------------------------------

#[test]
fn and001_debuggable_in_main_manifest_is_critical() {
    let report = run("and001_tp");
    assert_eq!(report.status, StoreStatus::Blocked);
    assert_eq!(report.critical, 1);
    assert_eq!(
        criticals(&report),
        vec![crit("AND-001", "app/src/main/AndroidManifest.xml", 4)]
    );
}

#[test]
fn and001_debuggable_in_release_manifest_is_critical() {
    let report = run("and001_release_tp");
    assert_eq!(
        criticals(&report),
        vec![crit("AND-001", "app/src/release/AndroidManifest.xml", 4)]
    );
}

#[test]
fn and001_debuggable_only_in_debug_and_test_source_sets_is_clean() {
    assert_clean("and001_tn_debug_only");
}

#[test]
fn and001_manifest_placeholder_is_unresolved() {
    assert_unresolved("and001_placeholder", "AND-001", 1);
}

#[test]
fn and001_flavor_manifest_is_unresolved_not_critical() {
    assert_unresolved("and001_flavor", "AND-001", 1);
}

// --- AND-002 ----------------------------------------------------------------

#[test]
fn and002_exported_matrix() {
    let report = run("and002_matrix");
    let file = "app/src/main/AndroidManifest.xml";
    assert_eq!(
        criticals(&report),
        vec![
            crit("AND-002", file, 5),
            crit("AND-002", file, 35),
            crit("AND-002", file, 40),
            crit("AND-002", file, 45),
        ]
    );
    let named: Vec<&str> = report
        .findings
        .iter()
        .filter(|f| f.rule == "AND-002")
        .filter_map(|f| f.message.split('"').nth(1))
        .collect();
    assert_eq!(
        named,
        vec![
            ".FilterNoExported",
            ".AliasNoExported",
            ".ServiceNoExported",
            ".ReceiverNoExported"
        ]
    );
    assert!(report.unresolved.is_empty(), "{:#?}", report.unresolved);
}

#[test]
fn and002_exported_supplied_by_overlay_manifest_is_clean() {
    assert_clean("and002_overlay");
}

#[test]
fn and002_unity_plugin_manifest_is_unresolved_not_critical() {
    // Found on a real Unity repo: Unity rewrites this manifest when it
    // generates the Gradle project, so a missing attribute is not certain.
    let report = assert_unresolved("and002_unity", "AND-002", 1);
    assert!(report.unresolved.iter().any(|u| u.starts_with("AND-003")));
    assert!(
        report
            .findings
            .iter()
            .all(|f| !f.file.starts_with("Library/")),
        "Unity's generated Library/ is not scanned"
    );
}

// --- AND-003 ----------------------------------------------------------------

#[test]
fn and003_literal_below_minimum_is_critical() {
    let report = run("and003_literal_low");
    assert_eq!(report.status, StoreStatus::Blocked);
    assert_eq!(
        criticals(&report),
        vec![crit("AND-003", "app/build.gradle.kts", 8)]
    );
    assert!(report.findings[0].message.contains("34"));
    assert!(report.findings[0].message.contains("36"));
}

#[test]
fn and003_literal_at_minimum_is_clean() {
    assert_clean("and003_literal_ok");
}

#[test]
fn and003_respects_configured_minimum() {
    let mut cfg = RepoConfig::default();
    cfg.gate.min_android_target_sdk = 34;
    let report = scan(&fixture("and003_literal_low"), &cfg, true);
    assert_eq!(report.status, StoreStatus::Clear);
    cfg.gate.min_android_target_sdk = 37;
    let report = scan(&fixture("and003_literal_ok"), &cfg, true);
    assert_eq!(report.status, StoreStatus::Blocked);
}

#[test]
fn and003_groovy_literal_is_critical() {
    let report = run("and003_groovy_low");
    assert_eq!(
        criticals(&report),
        vec![crit("AND-003", "app/build.gradle", 7)]
    );
}

#[test]
fn and003_commented_out_and_quoted_values_are_ignored() {
    assert_clean("and003_commented");
}

#[test]
fn and003_through_version_catalog() {
    let report = run("and003_catalog_low");
    assert_eq!(
        criticals(&report),
        vec![crit("AND-003", "app/build.gradle.kts", 9)]
    );
    assert_clean("and003_catalog_ok");
}

#[test]
fn and003_rich_catalog_version_is_unresolved() {
    assert_unresolved("and003_catalog_missing", "AND-003", 1);
}

#[test]
fn and003_through_ext_block() {
    let report = run("and003_ext_low");
    assert_eq!(
        criticals(&report),
        vec![crit("AND-003", "android/app/build.gradle", 7)]
    );
    assert_eq!(report.findings.len(), 1, "{:#?}", report.findings);
    assert_clean("and003_ext_ok");
}

#[test]
fn and003_through_gradle_properties() {
    let report = run("and003_props_low");
    assert_eq!(
        criticals(&report),
        vec![crit("AND-003", "app/build.gradle.kts", 8)]
    );
    assert_clean("and003_props_ok");
}

#[test]
fn and003_unresolvable_expressions_are_listed_never_critical() {
    assert_unresolved("and003_unresolvable", "AND-003", 4);
}

#[test]
fn and003_flutter_managed_value_is_unresolved() {
    let report = assert_unresolved("and003_flutter", "AND-003", 1);
    assert!(
        report.unresolved[0].contains("Flutter SDK"),
        "{:?}",
        report.unresolved
    );
}

#[test]
fn and003_library_and_test_modules_do_not_decide_the_target() {
    assert_clean("and003_library_low");
    assert_clean("and003_test_module");
}

#[test]
fn and003_unknown_module_type_is_unresolved() {
    assert_unresolved("and003_unknown_module", "AND-003", 1);
}

#[test]
fn and003_root_script_with_apply_false_is_not_an_application_module() {
    let report = assert_unresolved("and003_root_apply_false", "AND-003", 1);
    assert!(
        report.unresolved[0].contains("build.gradle:11"),
        "{:?}",
        report.unresolved
    );
}

#[test]
fn and003_cyclic_definitions_end_unresolved() {
    assert_unresolved("and003_cycle", "AND-003", 1);
}

#[test]
fn and003_convention_plugin() {
    assert_clean("and003_convention_ok");
    assert_unresolved("and003_convention_low", "AND-003", 1);
    assert_unresolved("and003_convention_missing", "AND-003", 1);
}

#[test]
fn and003_unity_project_settings() {
    let report = run("and003_unity_low");
    assert_eq!(
        criticals(&report),
        vec![crit("AND-003", "ProjectSettings/ProjectSettings.asset", 4)]
    );
}

// --- warnings ---------------------------------------------------------------

#[test]
fn android_warnings_do_not_block() {
    let report = run("android_warns");
    assert_eq!(report.status, StoreStatus::Clear);
    assert_eq!(report.critical, 0);
    assert_eq!(report.warnings, 4);
    assert_eq!(
        warn_rules(&report),
        vec!["AND-101", "AND-102", "AND-102", "AND-103"]
    );
    assert!(report.unresolved.is_empty());
    assert_clean("android_warns_tn");
}

// --- robustness -------------------------------------------------------------

#[test]
fn excluded_directories_are_not_scanned() {
    assert_clean("excluded_dirs");
}

#[test]
fn malformed_files_become_unresolved_and_do_not_hide_other_findings() {
    let report = run("malformed");
    assert_eq!(
        criticals(&report),
        vec![crit("AND-001", "app/src/main/AndroidManifest.xml", 4)]
    );
    let unresolved = report.unresolved.join("\n");
    assert!(
        unresolved.contains("STORE-000 broken/src/main/AndroidManifest.xml"),
        "{unresolved}"
    );
    assert!(
        unresolved.contains("STORE-000 notmanifest/src/main/AndroidManifest.xml"),
        "{unresolved}"
    );
    assert!(
        unresolved.contains("AND-003 other/build.gradle.kts:7"),
        "{unresolved}"
    );
    assert!(
        unresolved.contains("libs.versions.toml could not be parsed"),
        "{unresolved}"
    );
}
