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

/// Same shape as the `ios002_tp` fixture, compiling `Storage.swift`.
const STORAGE_PBXPROJ: &str = r#"// !$*UTF8*$!
{
	objects = {
		AA0000000000000000000001 /* Storage.swift */ = {isa = PBXFileReference; path = Storage.swift; sourceTree = "<group>"; };
		AA00000000000000000000Debug /* Debug */ = {
			isa = XCBuildConfiguration;
			buildSettings = {
				GENERATE_INFOPLIST_FILE = YES;
				IPHONEOS_DEPLOYMENT_TARGET = 17.0;
				SDKROOT = iphoneos;
			};
			name = Debug;
		};
		AA00000000000000000000Release /* Release */ = {
			isa = XCBuildConfiguration;
			buildSettings = {
				GENERATE_INFOPLIST_FILE = YES;
				IPHONEOS_DEPLOYMENT_TARGET = 17.0;
				SDKROOT = iphoneos;
			};
			name = Release;
		};
	};
}
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
    // The linked manifest names the platform, but neither link is read: no
    // Critical from the debuggable target, and both links are reported.
    assert_eq!(report.status, StoreStatus::Clear, "{report:#?}");
    assert_eq!(report.critical, 0, "{report:#?}");
    assert_eq!(report.warnings, 2, "{report:#?}");
    assert_eq!(
        report.unresolved,
        vec![
            unreadable("AndroidManifest.xml", "symbolic link is not followed"),
            unreadable("app", "symbolic link to a directory is not followed"),
        ],
        "{report:#?}"
    );

    // The same tree reached without a link is found.
    let report = scan(outside.path(), &RepoConfig::default(), true);
    assert_eq!(report.status, StoreStatus::Blocked);
}

/// Android library module: enough for the scanner to treat the tree as mobile.
#[cfg(unix)]
fn mobile_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    write(
        &tmp.path().join("lib/build.gradle"),
        "plugins { id 'com.android.library' }\n",
    );
    write(
        &tmp.path().join("lib/src/main/AndroidManifest.xml"),
        "<manifest xmlns:android=\"http://schemas.android.com/apk/res/android\" />\n",
    );
    tmp
}

#[cfg(unix)]
fn ln(src: impl AsRef<std::path::Path>, dst: impl AsRef<std::path::Path>) {
    std::os::unix::fs::symlink(src, dst).unwrap();
}

#[cfg(unix)]
fn unreadable(path: &str, reason: &str) -> String {
    format!("STORE-000 {path}: File could not be checked: {reason}.")
}

/// Exactly one STORE-000 warning, and the status it leaves.
#[cfg(unix)]
fn assert_only_link(report: &aaf_domain::gate::StoreComplianceReport, entry: &str) {
    assert_eq!(report.status, StoreStatus::Clear, "{report:#?}");
    assert_eq!(report.critical, 0, "{report:#?}");
    assert_eq!(report.warnings, 1, "{report:#?}");
    assert_eq!(report.unresolved, vec![entry.to_string()], "{report:#?}");
}

#[cfg(unix)]
#[test]
fn directory_symlink_is_unresolved_but_non_input_and_excluded_links_are_not() {
    let tmp = mobile_repo();
    let outside = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("app/src")).unwrap();
    write(&root.join("docs/notes.md"), "hello\n");
    write(
        &outside.path().join("android-main/AndroidManifest.xml"),
        DEBUGGABLE,
    );
    ln(
        outside.path().join("android-main"),
        root.join("app/src/main"),
    );
    ln(root.join("docs/notes.md"), root.join("README.md"));
    // Excluded by name, or inside a directory the walk never enters.
    ln(outside.path(), root.join("Pods"));
    fs::create_dir_all(root.join("node_modules")).unwrap();
    ln(outside.path(), root.join("node_modules/foo"));
    // Dangling: nothing to stat, stays silent.
    ln(outside.path().join("missing"), root.join("gone"));

    let report = scan(root, &RepoConfig::default(), true);
    let entry = unreadable(
        "app/src/main",
        "symbolic link to a directory is not followed",
    );
    assert_only_link(&report, &entry);
}

#[cfg(unix)]
#[test]
fn symlink_that_cannot_be_stated_is_unresolved_unless_its_directory_is_skipped() {
    let tmp = mobile_repo();
    let root = tmp.path();
    fs::create_dir_all(root.join("app")).unwrap();
    // `a -> a`: stat fails with ELOOP, not NotFound.
    ln("loop", root.join("app/loop"));
    fs::create_dir_all(root.join("node_modules")).unwrap();
    ln("loop", root.join("node_modules/loop"));
    ln("Pods", root.join("Pods"));

    let report = scan(root, &RepoConfig::default(), true);
    assert_eq!(report.status, StoreStatus::Clear, "{report:#?}");
    assert_eq!(report.critical, 0, "{report:#?}");
    assert_eq!(report.warnings, 1, "{report:#?}");
    assert_eq!(report.unresolved.len(), 1, "{report:#?}");
    // The OS error text differs between platforms; the rest is exact.
    let prefix =
        "STORE-000 app/loop: File could not be checked: symbolic link could not be checked: ";
    assert!(report.unresolved[0].starts_with(prefix), "{report:#?}");
}

#[cfg(unix)]
#[test]
fn directory_links_the_walk_would_skip_as_directories_are_not_reported() {
    let tmp = mobile_repo();
    let outside = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("app/shared-res")).unwrap();
    fs::create_dir_all(root.join("app/src/main")).unwrap();
    fs::create_dir_all(root.join("rust")).unwrap();
    write(
        &outside.path().join("cargo-target/CACHEDIR.TAG"),
        "Signature: 8a477f597d28d172789f06886806bc55\n",
    );
    ln("../../shared-res", root.join("app/src/main/res"));
    ln(
        outside.path().join("cargo-target"),
        root.join("rust/target"),
    );
    // Walked as a directory, so the link is reported.
    ln("../../shared-res", root.join("app/src/main/assets"));

    let report = scan(root, &RepoConfig::default(), true);
    let entry = unreadable(
        "app/src/main/assets",
        "symbolic link to a directory is not followed",
    );
    assert_only_link(&report, &entry);
}

#[cfg(unix)]
#[test]
fn linked_inputs_their_scanner_skips_are_not_reported() {
    let tmp = mobile_repo();
    let root = tmp.path();
    let manifest = root.join("lib/src/main/AndroidManifest.xml");
    write(&root.join("ios/App/Real.swift"), "import Foundation\n");
    for rel in [
        "app/src/debug/AndroidManifest.xml",
        "app/src/test/AndroidManifest.xml",
        "app/src/androidTest/AndroidManifest.xml",
        "app/src/main/AndroidManifest.xml",
    ] {
        fs::create_dir_all(root.join(rel).parent().unwrap()).unwrap();
        ln(&manifest, root.join(rel));
    }
    fs::create_dir_all(root.join("ios/AppTests")).unwrap();
    ln(
        root.join("ios/App/Real.swift"),
        root.join("ios/AppTests/Mock.swift"),
    );

    let report = scan(root, &RepoConfig::default(), true);
    let entry = unreadable(
        "app/src/main/AndroidManifest.xml",
        "symbolic link is not followed",
    );
    assert_only_link(&report, &entry);
}

#[cfg(unix)]
#[test]
fn linked_kotlin_is_reported_only_in_an_apple_source_set() {
    let tmp = mobile_repo();
    let root = tmp.path();
    write(&root.join("shared/Real.kt"), "package p\n");
    for set in ["iosMain", "commonMain"] {
        let dir = root.join(format!("shared/src/{set}/kotlin/p"));
        fs::create_dir_all(&dir).unwrap();
        ln(root.join("shared/Real.kt"), dir.join("A.kt"));
    }

    let report = scan(root, &RepoConfig::default(), true);
    let entry = unreadable(
        "shared/src/iosMain/kotlin/p/A.kt",
        "symbolic link is not followed",
    );
    assert_only_link(&report, &entry);
}

#[cfg(unix)]
#[test]
fn symlinked_apple_source_is_unresolved_not_dropped() {
    let tmp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    write(
        &tmp.path().join("ios/App.xcodeproj/project.pbxproj"),
        STORAGE_PBXPROJ,
    );
    let source = "import Foundation\n\nfunc seen() -> Bool {\n    UserDefaults.standard.bool(forKey: \"seen\")\n}\n";
    write(&outside.path().join("Storage.swift"), source);
    let link = tmp.path().join("ios/App/Storage.swift");
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    ln(outside.path().join("Storage.swift"), &link);

    let report = scan(tmp.path(), &RepoConfig::default(), true);
    let entry = unreadable("ios/App/Storage.swift", "symbolic link is not followed");
    assert_only_link(&report, &entry);

    // The same file in place is read and blocks (IOS-002).
    fs::remove_file(&link).unwrap();
    write(&link, source);
    let report = scan(tmp.path(), &RepoConfig::default(), true);
    assert_eq!(report.status, StoreStatus::Blocked, "{report:#?}");
    assert_eq!(report.critical, 1, "{report:#?}");
    assert!(report.unresolved.is_empty(), "{report:#?}");
    let critical: Vec<(&str, &str)> = report
        .findings
        .iter()
        .filter(|f| f.severity == Severity::Critical)
        .map(|f| (f.rule.as_str(), f.file.as_str()))
        .collect();
    assert_eq!(critical, vec![("IOS-002", "ios/App/Storage.swift")]);
}

#[cfg(unix)]
#[test]
fn non_mobile_repo_with_symlinked_package_json_is_not_applicable_without_unresolved() {
    let tmp = tempfile::tempdir().unwrap();
    write(&tmp.path().join("target.json"), r#"{"name":"web-app"}"#);
    ln(
        tmp.path().join("target.json"),
        tmp.path().join("package.json"),
    );

    let report = scan(tmp.path(), &RepoConfig::default(), true);
    assert_eq!(report.status, StoreStatus::NotApplicable, "{report:#?}");
    assert!(report.unresolved.is_empty(), "{report:#?}");
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
