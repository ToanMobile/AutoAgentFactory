#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Activation, tree walk bounds and report invariants of the store scanner.

use std::fs;
use std::path::{Path, PathBuf};

use aaf_domain::config::{RepoConfig, StoreMode};
use aaf_domain::gate::{Severity, StoreStatus};
use aaf_engine::store_compliance::scan;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/store")
}

fn cfg(mode: StoreMode, profile: &str) -> RepoConfig {
    let mut cfg = RepoConfig::default();
    cfg.gate.store_compliance = mode;
    cfg.project.profile = profile.to_string();
    cfg
}

const DEBUGGABLE: &str = r#"<manifest xmlns:android="http://schemas.android.com/apk/res/android">
    <application android:debuggable="true" />
</manifest>
"#;

fn write(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

#[test]
fn off_is_skipped_even_for_a_blocked_project() {
    let report = scan(
        &fixtures().join("and001_tp"),
        &cfg(StoreMode::Off, "auto"),
        true,
    );
    assert_eq!(report.status, StoreStatus::Skipped);
    assert!(report.findings.is_empty());
}

#[test]
fn repo_without_mobile_project_is_not_applicable() {
    for name in ["not_mobile", "not_expo"] {
        for mode in [StoreMode::Auto, StoreMode::Always] {
            let report = scan(&fixtures().join(name), &cfg(mode, "auto"), true);
            assert_eq!(report.status, StoreStatus::NotApplicable, "{name} {mode:?}");
        }
    }
}

#[test]
fn missing_root_is_not_applicable_with_the_reason_listed() {
    let report = scan(
        &fixtures().join("does_not_exist"),
        &RepoConfig::default(),
        true,
    );
    assert_eq!(report.status, StoreStatus::NotApplicable);
    assert_eq!(report.unresolved.len(), 1, "{:?}", report.unresolved);
}

/// Applicability comes from the repository content: a non-mobile profile does
/// not hide a detected mobile project, in `auto` or in `always`.
#[test]
fn a_non_mobile_profile_does_not_suppress_a_detected_mobile_project() {
    let root = fixtures().join("and001_tp");
    for profile in ["rust", "web", "backend", "RUST"] {
        let report = scan(&root, &cfg(StoreMode::Auto, profile), true);
        assert_eq!(report.status, StoreStatus::Blocked, "{profile}");
        let report = scan(&root, &cfg(StoreMode::Always, profile), true);
        assert_eq!(report.status, StoreStatus::Blocked, "{profile}");
    }
    for profile in ["auto", "android", "unity", "flutter"] {
        let report = scan(&root, &cfg(StoreMode::Auto, profile), true);
        assert_eq!(report.status, StoreStatus::Blocked, "{profile}");
    }
    // Allowed side: no mobile project, whatever the profile claims.
    for profile in ["rust", "android"] {
        let report = scan(
            &fixtures().join("not_mobile"),
            &cfg(StoreMode::Auto, profile),
            true,
        );
        assert_eq!(report.status, StoreStatus::NotApplicable, "{profile}");
    }
}

#[test]
fn release_flag_does_not_change_the_findings() {
    let root = fixtures().join("and001_tp");
    let cfg = RepoConfig::default();
    assert_eq!(scan(&root, &cfg, true), scan(&root, &cfg, false));
}

#[cfg(unix)]
#[test]
fn symlinks_are_not_followed() {
    let tmp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    write(
        &outside.path().join("app/src/main/AndroidManifest.xml"),
        DEBUGGABLE,
    );
    std::os::unix::fs::symlink(outside.path().join("app"), tmp.path().join("app")).unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("app/src/main/AndroidManifest.xml"),
        tmp.path().join("AndroidManifest.xml"),
    )
    .unwrap();
    let report = scan(tmp.path(), &RepoConfig::default(), true);
    assert_eq!(report.status, StoreStatus::NotApplicable, "{report:#?}");

    // The same tree reached without a link is found.
    let report = scan(outside.path(), &RepoConfig::default(), true);
    assert_eq!(report.status, StoreStatus::Blocked);
}

#[test]
fn walk_depth_is_bounded_and_the_cut_is_reported() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        &tmp.path().join("app/src/main/AndroidManifest.xml"),
        "<manifest/>",
    );
    let mut deep = tmp.path().to_path_buf();
    for i in 0..30 {
        deep.push(format!("d{i}"));
    }
    write(&deep.join("src/main/AndroidManifest.xml"), DEBUGGABLE);
    let report = scan(tmp.path(), &RepoConfig::default(), true);
    assert_eq!(report.critical, 0, "{report:#?}");
    assert_eq!(report.unresolved.len(), 1);
    assert!(
        report.unresolved[0].contains("not scanned"),
        "{:?}",
        report.unresolved
    );
}

#[cfg(unix)]
#[test]
fn unreadable_file_is_unresolved_and_the_scan_continues() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    write(
        &tmp.path().join("app/src/main/AndroidManifest.xml"),
        DEBUGGABLE,
    );
    let locked = tmp.path().join("locked/src/main/AndroidManifest.xml");
    write(&locked, DEBUGGABLE);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(&locked).is_ok() {
        // Running as root: permissions do not apply, nothing to prove here.
        return;
    }
    let report = scan(tmp.path(), &RepoConfig::default(), true);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(report.critical, 1);
    assert_eq!(report.unresolved.len(), 1, "{:?}", report.unresolved);
    assert!(report.unresolved[0].starts_with("STORE-000 locked/src/main/AndroidManifest.xml"));
}

/// Every fixture: the scan never panics, counts match the findings, and each
/// finding carries a file, a message, a fix and its guideline.
#[test]
fn every_finding_is_complete_and_counts_are_consistent() {
    let mut scanned = 0;
    let mut dirs: Vec<_> = fs::read_dir(fixtures())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    for dir in dirs {
        let report = scan(&dir, &cfg(StoreMode::Always, "auto"), true);
        scanned += 1;
        let count = |s: Severity| report.findings.iter().filter(|f| f.severity == s).count();
        assert_eq!(
            report.critical as usize,
            count(Severity::Critical),
            "{dir:?}"
        );
        assert_eq!(report.warnings as usize, count(Severity::Warn), "{dir:?}");
        let blocked = report.status == StoreStatus::Blocked;
        assert_eq!(blocked, report.critical > 0, "{dir:?}");
        for f in &report.findings {
            assert!(!f.file.is_empty() && !f.file.starts_with('/'), "{f:?}");
            assert!(
                f.message.ends_with('.') || f.message.ends_with(')'),
                "{f:?}"
            );
            assert!(!f.fix.is_empty() && !f.guideline.is_empty(), "{f:?}");
            assert!(
                matches!(f.platform.as_str(), "android" | "ios" | "expo" | "any"),
                "{f:?}"
            );
            assert!(f.line != Some(0), "{f:?}");
        }
        // Unresolved entries are warnings too, so nothing is only in one list.
        assert!(report.unresolved.len() <= count(Severity::Warn), "{dir:?}");
        // Deterministic: a second run gives the same report.
        assert_eq!(
            report,
            scan(&dir, &cfg(StoreMode::Always, "auto"), true),
            "{dir:?}"
        );
    }
    assert!(scanned >= 55, "only {scanned} fixtures found");
}
