#![allow(clippy::unwrap_used, clippy::expect_used)]
//! iOS and Expo rules of the store scanner against `tests/fixtures/store`.

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

/// Not blocked; `rule` is listed `times` times as unresolved and nothing else is.
fn assert_only_unresolved(name: &str, rule: &str, times: usize) -> StoreComplianceReport {
    let report = run(name);
    assert_eq!(criticals(&report), vec![], "{name}");
    assert_eq!(report.status, StoreStatus::Clear, "{name}");
    let listed = report
        .unresolved
        .iter()
        .filter(|u| u.starts_with(rule))
        .count();
    assert_eq!(listed, times, "{name}: {:#?}", report.unresolved);
    assert_eq!(
        report.unresolved.len(),
        times,
        "{name}: {:#?}",
        report.unresolved
    );
    assert_eq!(
        report.findings.len(),
        times,
        "{name}: {:#?}",
        report.findings
    );
    assert!(report.findings.iter().all(|f| f.severity == Severity::Warn));
    report
}

// --- IOS-001 ----------------------------------------------------------------

#[test]
fn ios001_empty_and_placeholder_purpose_strings_are_critical() {
    let report = run("ios001_plist_tp");
    assert_eq!(report.status, StoreStatus::Blocked);
    let file = "App/Info.plist";
    // One per key, in file order: lines 7, 9, ... 21.
    let expected: Vec<_> = (0..8).map(|i| crit("IOS-001", file, 7 + 2 * i)).collect();
    assert_eq!(criticals(&report), expected);
    assert_eq!(report.critical, 8);
}

#[test]
fn ios001_sentences_that_contain_placeholder_words_are_clean() {
    // "to test your connection", "Todo lists", "a description", "placeholder
    // pin", "Testing who to invite" are legitimate purpose strings.
    assert_clean("ios001_plist_tn");
}

#[test]
fn ios001_build_setting_values_are_unresolved() {
    assert_only_unresolved("ios001_variable", "IOS-001", 2);
}

#[test]
fn ios001_purpose_string_only_in_pbxproj() {
    let report = run("ios001_pbxproj_tp");
    let file = "App.xcodeproj/project.pbxproj";
    // Debug and Release repeat the settings; each is reported once.
    assert_eq!(
        criticals(&report),
        vec![crit("IOS-001", file, 10), crit("IOS-001", file, 11)]
    );
    assert_only_unresolved("ios001_pbxproj_tn", "IOS-001", 1);
}

#[test]
fn ios001_unreferenced_plist_is_unresolved_not_critical() {
    assert_only_unresolved("ios001_unreferenced", "IOS-001", 1);
}

#[test]
fn ios001_localized_purpose_string_is_unresolved_not_critical() {
    assert_only_unresolved("ios001_localized", "IOS-001", 1);
}

#[test]
fn ios001_plist_paths_that_cannot_be_followed_are_unresolved() {
    assert_only_unresolved("ios001_missing_plist", "IOS-001", 3);
}

#[test]
fn ios001_malformed_plist_is_unresolved() {
    let report = assert_only_unresolved("ios001_bad_plist", "STORE-000", 1);
    assert!(report.unresolved[0].contains("App/Info.plist"));
}

#[test]
fn ios001_compiled_bundles_and_pods_are_not_scanned() {
    assert_clean("ios001_bundles");
}

#[test]
fn ios102_short_purpose_string_warns() {
    let report = run("ios102_short");
    assert_eq!(report.status, StoreStatus::Clear);
    assert_eq!(report.warnings, 1);
    assert_eq!(report.findings[0].rule, "IOS-102");
    assert_eq!(report.findings[0].line, Some(7));
    assert!(report.unresolved.is_empty());
}

// --- IOS-002 / IOS-101 --------------------------------------------------------

#[test]
fn ios002_required_reason_api_without_privacy_manifest_is_critical() {
    let report = run("ios002_tp");
    assert_eq!(report.status, StoreStatus::Blocked);
    assert_eq!(
        criticals(&report),
        vec![
            crit("IOS-002", "App/AppDelegate.swift", 5),
            crit("IOS-002", "App/Clock.m", 4),
        ]
    );
}

#[test]
fn ios002_privacy_manifest_present_is_clean() {
    assert_clean("ios002_tn_manifest");
}

#[test]
fn ios101_manifest_without_the_used_category_warns() {
    let report = run("ios101_missing_category");
    assert_eq!(report.status, StoreStatus::Clear);
    assert_eq!(report.warnings, 1);
    assert_eq!(report.findings[0].rule, "IOS-101");
    assert!(
        report.findings[0]
            .message
            .contains("NSPrivacyAccessedAPICategoryUserDefaults")
    );
    assert!(report.unresolved.is_empty());
}

#[test]
fn ios002_unreadable_privacy_manifest_is_unresolved() {
    assert_only_unresolved("ios002_bad_manifest", "STORE-000", 1);
}

#[test]
fn ios002_token_only_in_pods_and_other_excluded_directories_is_clean() {
    assert_clean("ios002_pods_only");
}

#[test]
fn ios002_token_only_in_comments_strings_and_longer_identifiers_is_clean() {
    assert_clean("ios002_comment_string");
}

#[test]
fn ios002_change_notification_name_is_not_an_access() {
    assert_clean("ios002_notification_only");
}

#[test]
fn ios002_token_only_in_test_targets_is_clean() {
    assert_clean("ios002_tests_only");
}

#[test]
fn ios002_file_not_compiled_by_the_project_is_unresolved() {
    assert_only_unresolved("ios002_unlisted", "IOS-002", 1);
}

#[test]
fn ios002_macos_only_project_is_not_judged() {
    assert_clean("ios002_macos");
}

// --- Expo ---------------------------------------------------------------------

#[test]
fn expo_app_json_target_sdk_and_purpose_string_are_critical() {
    let report = run("expo_tp");
    assert_eq!(report.status, StoreStatus::Blocked);
    assert_eq!(
        criticals(&report),
        vec![
            crit("AND-003", "app.json", 11),
            crit("IOS-001", "app.json", 18)
        ]
    );
}

#[test]
fn expo_valid_app_json_is_clean() {
    assert_clean("expo_tn");
}

#[test]
fn expo_dynamic_config_downgrades_to_unresolved() {
    let report = run("expo_dynamic");
    assert_eq!(criticals(&report), vec![]);
    assert_eq!(report.status, StoreStatus::Clear);
    let unresolved = report.unresolved.join("\n");
    assert!(unresolved.contains("AND-003 app.json:11"), "{unresolved}");
    assert!(unresolved.contains("IOS-001 app.json:18"), "{unresolved}");
    assert!(unresolved.contains("IOS-001 app.config.js"), "{unresolved}");
    assert_eq!(report.unresolved.len(), 3);
}

#[test]
fn expo_default_target_sdk_and_variable_are_unresolved() {
    let report = run("expo_default");
    assert_eq!(criticals(&report), vec![]);
    let unresolved = report.unresolved.join("\n");
    assert!(unresolved.contains("AND-003 app.json"), "{unresolved}");
    assert!(unresolved.contains("IOS-001 app.json:1"), "{unresolved}");
    assert_eq!(report.unresolved.len(), 2);
}

#[test]
fn expo_malformed_app_json_is_unresolved() {
    assert_only_unresolved("expo_bad_json", "STORE-000", 1);
}
