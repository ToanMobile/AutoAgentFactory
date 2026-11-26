#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]
//! Verification test for GATE-064: symlinked Android manifest must not be
//! silently dropped during `store_compliance::scan`.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

use aaf_domain::config::RepoConfig;
use aaf_domain::gate::StoreStatus;
use aaf_engine::store_compliance::scan;
use tempfile::TempDir;

const LIB_BUILD_GRADLE: &str = r#"plugins {
    id 'com.android.library'
}
"#;

const CLEAN_MANIFEST: &str = r#"<manifest xmlns:android="http://schemas.android.com/apk/res/android" />
"#;

const APP_BUILD_GRADLE: &str = r#"plugins {
    id 'com.android.application'
}

android {
    defaultConfig {
        targetSdk = 36
    }
}
"#;

const DEBUGGABLE_MANIFEST: &str = r#"<manifest xmlns:android="http://schemas.android.com/apk/res/android">
    <application android:debuggable="true" />
</manifest>
"#;

fn write(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn create_two_module_android_repo() -> TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(&root.join("lib/build.gradle"), LIB_BUILD_GRADLE);
    write(
        &root.join("lib/src/main/AndroidManifest.xml"),
        CLEAN_MANIFEST,
    );
    write(&root.join("app/build.gradle"), APP_BUILD_GRADLE);
    write(&root.join("app/manifest-main.xml"), DEBUGGABLE_MANIFEST);
    tmp
}

#[test]
fn symlinked_manifest_is_reported_unresolved_not_silently_skipped() {
    let tmp = create_two_module_android_repo();
    let manifest_link = tmp.path().join("app/src/main/AndroidManifest.xml");
    fs::create_dir_all(manifest_link.parent().unwrap()).unwrap();
    symlink(Path::new("../../manifest-main.xml"), &manifest_link).unwrap();
    assert!(
        manifest_link.exists(),
        "test fixture symlink must resolve to app/manifest-main.xml"
    );

    let report = scan(tmp.path(), &RepoConfig::default(), true);

    assert_ne!(
        report.status,
        StoreStatus::NotApplicable,
        "fixture must detect Android platform; status={:?}, unresolved={:#?}",
        report.status,
        report.unresolved
    );
    assert_eq!(
        report.critical, 0,
        "symlinks must not be followed into Critical findings; status={:?}, unresolved={:#?}, findings={:#?}",
        report.status, report.unresolved, report.findings
    );
    assert!(
        report
            .unresolved
            .iter()
            .any(|entry| entry.contains("app/src/main/AndroidManifest.xml")),
        "symlinked manifest was silently skipped instead of recorded in unresolved; status={:?}, unresolved={:#?}",
        report.status,
        report.unresolved
    );
}

#[test]
fn regular_manifest_with_debuggable_is_critical() {
    let tmp = create_two_module_android_repo();
    write(
        &tmp.path().join("app/src/main/AndroidManifest.xml"),
        DEBUGGABLE_MANIFEST,
    );

    let report = scan(tmp.path(), &RepoConfig::default(), true);

    assert!(
        report.critical >= 1,
        "expected regular debuggable manifest to trigger Critical finding; status={:?}, critical={}, findings={:#?}",
        report.status,
        report.critical,
        report.findings
    );
}
