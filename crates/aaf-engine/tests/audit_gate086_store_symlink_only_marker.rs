#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]
//! Verification test for GATE-086: when a repository's only platform marker
//! (`AndroidManifest.xml`, `project.pbxproj`, or `ProjectSettings/ProjectSettings.asset`)
//! is a symlink, `store_compliance::scan` must not silently drop it as
//! `StoreStatus::NotApplicable`.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

use aaf_domain::config::RepoConfig;
use aaf_domain::gate::StoreStatus;
use aaf_engine::store_compliance::scan;

const DEBUGGABLE_MANIFEST: &str = r#"<manifest xmlns:android="http://schemas.android.com/apk/res/android">
    <application android:debuggable="true" />
</manifest>
"#;

const MINIMAL_PBXPROJ: &str = r#"// !$*UTF8*$!
{
	objects = {
		AA000000000000000000Release /* Release */ = {
			isa = XCBuildConfiguration;
			buildSettings = {
				IPHONEOS_DEPLOYMENT_TARGET = 17.0;
				SDKROOT = iphoneos;
			};
			name = Release;
		};
	};
}
"#;

const MINIMAL_UNITY_SETTINGS: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!129 &1
PlayerSettings:
  productName: DemoGame
"#;

fn write(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn ln(src: &Path, dst: &Path) {
    fs::create_dir_all(dst.parent().unwrap()).unwrap();
    symlink(src, dst).unwrap();
    assert!(
        dst.exists(),
        "test fixture symlink {} must resolve to {}",
        dst.display(),
        src.display()
    );
}

#[test]
fn symlink_only_android_manifest_is_not_silently_dropped() {
    let repo = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("AndroidManifest.xml");
    write(&target, DEBUGGABLE_MANIFEST);

    let link = repo.path().join("app/src/main/AndroidManifest.xml");
    ln(&target, &link);

    let report = scan(repo.path(), &RepoConfig::default(), true);

    assert_eq!(
        report.status,
        StoreStatus::Clear,
        "symlink-only AndroidManifest.xml must not collapse to NotApplicable; report={report:#?}"
    );
    assert_eq!(
        report.critical, 0,
        "symlinks must not be followed into Critical findings; report={report:#?}"
    );
    assert!(
        report.warnings >= 1,
        "expected at least one warning for symlinked marker; report={report:#?}"
    );
    assert!(
        report
            .unresolved
            .iter()
            .any(|e| e.starts_with("STORE-000") && e.contains("app/src/main/AndroidManifest.xml")),
        "expected STORE-000 unresolved entry for app/src/main/AndroidManifest.xml; report={report:#?}"
    );
}

#[test]
fn symlink_only_ios_pbxproj_is_not_silently_dropped() {
    let repo = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("project.pbxproj");
    write(&target, MINIMAL_PBXPROJ);

    let link = repo.path().join("App.xcodeproj/project.pbxproj");
    ln(&target, &link);

    let report = scan(repo.path(), &RepoConfig::default(), true);

    assert_eq!(
        report.status,
        StoreStatus::Clear,
        "symlink-only project.pbxproj must not collapse to NotApplicable; report={report:#?}"
    );
    assert_eq!(
        report.critical, 0,
        "symlinks must not be followed into Critical findings; report={report:#?}"
    );
    assert!(
        report.warnings >= 1,
        "expected at least one warning for symlinked marker; report={report:#?}"
    );
    assert!(
        report
            .unresolved
            .iter()
            .any(|e| e.starts_with("STORE-000") && e.contains("App.xcodeproj/project.pbxproj")),
        "expected STORE-000 unresolved entry for App.xcodeproj/project.pbxproj; report={report:#?}"
    );
}

#[test]
fn symlink_only_unity_project_settings_is_not_silently_dropped() {
    let repo = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("ProjectSettings.asset");
    write(&target, MINIMAL_UNITY_SETTINGS);

    let link = repo.path().join("ProjectSettings/ProjectSettings.asset");
    ln(&target, &link);

    let report = scan(repo.path(), &RepoConfig::default(), true);

    assert_eq!(
        report.status,
        StoreStatus::Clear,
        "symlink-only ProjectSettings.asset must not collapse to NotApplicable; report={report:#?}"
    );
    assert_eq!(
        report.critical, 0,
        "symlinks must not be followed into Critical findings; report={report:#?}"
    );
    assert!(
        report.warnings >= 1,
        "expected at least one warning for symlinked marker; report={report:#?}"
    );
    assert!(
        report.unresolved.iter().any(|e| {
            e.starts_with("STORE-000") && e.contains("ProjectSettings/ProjectSettings.asset")
        }),
        "expected STORE-000 unresolved entry for ProjectSettings/ProjectSettings.asset; report={report:#?}"
    );
}

#[test]
fn non_mobile_repo_with_non_marker_symlink_stays_not_applicable() {
    let repo = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("README.md");
    write(&target, "# Plain repository\n");

    let link = repo.path().join("README.md");
    ln(&target, &link);

    let report = scan(repo.path(), &RepoConfig::default(), true);

    assert_eq!(report.status, StoreStatus::NotApplicable, "{report:#?}");
    assert_eq!(report.warnings, 0, "{report:#?}");
    assert!(report.unresolved.is_empty(), "{report:#?}");
}

#[test]
fn non_mobile_repo_with_skipped_debug_manifest_symlink_stays_not_applicable() {
    let repo = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("AndroidManifest.xml");
    write(&target, DEBUGGABLE_MANIFEST);

    let link = repo.path().join("app/src/debug/AndroidManifest.xml");
    ln(&target, &link);

    let report = scan(repo.path(), &RepoConfig::default(), true);

    assert_eq!(report.status, StoreStatus::NotApplicable, "{report:#?}");
    assert_eq!(report.warnings, 0, "{report:#?}");
    assert!(report.unresolved.is_empty(), "{report:#?}");
}
