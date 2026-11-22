#![allow(clippy::unwrap_used, clippy::expect_used)]
//! IOS-002 / IOS-101 for Kotlin Multiplatform code compiled for iOS
//! (`src/iosMain/**/*.kt`), on a layout cut down from a real KMP app: an
//! Xcode project that links the framework of one Gradle module, and a second
//! module that no Xcode project in the repository names.

use std::fs;

use aaf_domain::config::RepoConfig;
use aaf_domain::gate::{Severity, StoreComplianceReport, StoreStatus};
use aaf_engine::store_compliance::scan;
use tempfile::TempDir;

const PBXPROJ: &str = r#"// !$*UTF8*$!
{
	objects = {
		AA01 /* iOSApp.swift */ = {isa = PBXFileReference; path = iOSApp.swift; sourceTree = "<group>"; };
		AA02 /* ComposeUi.framework */ = {isa = PBXFileReference; lastKnownFileType = wrapper.framework; name = ComposeUi.framework; path = "composeUi/build/xcode-frameworks/$(CONFIGURATION)/$(SDK_NAME)/ComposeUi.framework"; sourceTree = "<group>"; };
		AA03 /* Release */ = {
			isa = XCBuildConfiguration;
			buildSettings = {
				FRAMEWORK_SEARCH_PATHS = "$(SRCROOT)/../composeUi/build/xcode-frameworks/$(CONFIGURATION)/$(SDK_NAME)";
				GENERATE_INFOPLIST_FILE = YES;
				IPHONEOS_DEPLOYMENT_TARGET = 17.0;
				SDKROOT = iphoneos;
			};
			name = Release;
		};
	};
}
"#;

const SWIFT_APP: &str = "import SwiftUI\n\n@main\nstruct iOSApp: App {\n    var body: some Scene { WindowGroup { Text(\"x\") } }\n}\n";

const PREFS_KT: &str = "package p\n\nimport platform.Foundation.NSUserDefaults\n\n// NSFileModificationDate is not read here.\nfun theme(): String? = NSUserDefaults.standardUserDefaults.stringForKey(\"theme\")\n";

const MEDIA_KT: &str = "package p\n\nimport platform.Foundation.NSDate\nimport platform.Foundation.NSFileManager\n\nfun modified(fm: NSFileManager, path: String): NSDate? =\n    fm.attributesOfItemAtPath(path, null)?.get(\"NSFileModificationDate\") as? NSDate\n";

const PREFS_REL: &str = "PhoneConnect/composeUi/src/iosMain/kotlin/p/Prefs.ios.kt";
const MEDIA_REL: &str = "PhoneConnect/composeUi/src/iosMain/kotlin/p/Media.kt";

fn manifest(categories: &[&str]) -> String {
    let entries: String = categories
        .iter()
        .map(|c| {
            format!(
                "<dict><key>NSPrivacyAccessedAPIType</key><string>NSPrivacyAccessedAPICategory{c}</string>\
                 <key>NSPrivacyAccessedAPITypeReasons</key><array><string>CA92.1</string></array></dict>"
            )
        })
        .collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\
         <key>NSPrivacyAccessedAPITypes</key><array>{entries}</array></dict></plist>\n"
    )
}

/// The app shell plus `extra` files.
fn app(extra: &[(&str, &str)]) -> TempDir {
    let dir = TempDir::new().unwrap();
    let base = [
        (
            "PhoneConnect/iosApp/iosApp.xcodeproj/project.pbxproj",
            PBXPROJ,
        ),
        ("PhoneConnect/iosApp/iosApp/iOSApp.swift", SWIFT_APP),
        (
            "PhoneConnect/composeUi/build.gradle.kts",
            "kotlin { iosArm64() }\n",
        ),
    ];
    for (rel, body) in base.iter().chain(extra) {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
    dir
}

fn run(extra: &[(&str, &str)]) -> StoreComplianceReport {
    scan(app(extra).path(), &RepoConfig::default(), true)
}

fn found(report: &StoreComplianceReport, severity: Severity) -> Vec<(String, String, Option<u32>)> {
    let mut found: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.severity == severity)
        .map(|f| (f.rule.clone(), f.file.clone(), f.line))
        .collect();
    found.sort();
    found
}

fn at(rule: &str, file: &str, line: u32) -> (String, String, Option<u32>) {
    (rule.to_string(), file.to_string(), Some(line))
}

fn assert_clean(what: &str, extra: &[(&str, &str)]) {
    let report = run(extra);
    assert_eq!(report.status, StoreStatus::Clear, "{what}: {report:#?}");
    assert!(report.findings.is_empty(), "{what}: {:#?}", report.findings);
    assert!(
        report.unresolved.is_empty(),
        "{what}: {:#?}",
        report.unresolved
    );
}

#[test]
fn ios_main_kotlin_using_required_reason_apis_without_a_manifest_is_critical() {
    let report = run(&[(PREFS_REL, PREFS_KT), (MEDIA_REL, MEDIA_KT)]);
    assert_eq!(report.status, StoreStatus::Blocked);
    assert_eq!(
        found(&report, Severity::Critical),
        vec![at("IOS-002", MEDIA_REL, 7), at("IOS-002", PREFS_REL, 6)]
    );
    let messages: Vec<&str> = report.findings.iter().map(|f| f.message.as_str()).collect();
    assert!(
        messages
            .iter()
            .any(|m| m.contains("NSFileModificationDate")),
        "{messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("NSUserDefaults")),
        "{messages:?}"
    );
}

#[test]
fn manifest_of_the_xcode_project_that_links_the_module_covers_the_kotlin_code() {
    let declared = manifest(&["UserDefaults", "FileTimestamp"]);
    assert_clean(
        "both categories declared",
        &[
            (PREFS_REL, PREFS_KT),
            (MEDIA_REL, MEDIA_KT),
            (
                "PhoneConnect/iosApp/iosApp/PrivacyInfo.xcprivacy",
                &declared,
            ),
        ],
    );
    // A manifest bundled with the Kotlin module counts as well.
    assert_clean(
        "manifest in the module",
        &[
            (PREFS_REL, PREFS_KT),
            (MEDIA_REL, MEDIA_KT),
            (
                "PhoneConnect/composeUi/src/iosMain/resources/PrivacyInfo.xcprivacy",
                &declared,
            ),
        ],
    );
}

#[test]
fn manifest_without_the_category_the_kotlin_code_uses_is_a_warning() {
    let report = run(&[
        (PREFS_REL, PREFS_KT),
        (MEDIA_REL, MEDIA_KT),
        (
            "PhoneConnect/iosApp/iosApp/PrivacyInfo.xcprivacy",
            &manifest(&["UserDefaults"]),
        ),
    ]);
    assert_eq!(report.status, StoreStatus::Clear);
    assert_eq!(found(&report, Severity::Critical), vec![]);
    assert_eq!(
        found(&report, Severity::Warn),
        vec![at("IOS-101", MEDIA_REL, 7)]
    );
}

#[test]
fn every_category_is_recognised_through_kotlin_native_names() {
    let kt = "package p\n\n\
        import kotlinx.cinterop.*\n\
        import platform.Foundation.*\n\
        import platform.UIKit.UITextInputMode\n\
        import platform.posix.stat\n\n\
        fun a() = NSUserDefaults.standardUserDefaults\n\
        fun b(path: String) = memScoped { val s = alloc<stat>(); stat(path, s.ptr) }\n\
        fun c() = NSProcessInfo.processInfo.systemUptime\n\
        fun d(fm: NSFileManager) = fm.attributesOfFileSystemForPath(\"/\", null)?.get(NSFileSystemFreeSize)\n\
        fun e() = UITextInputMode.activeInputModes\n\
        fun f() = platform.darwin.mach_absolute_time()\n";
    let rel = "PhoneConnect/composeUi/src/iosMain/kotlin/p/All.kt";
    let report = run(&[(rel, kt)]);
    // One finding per category, at its first use.
    assert_eq!(
        found(&report, Severity::Critical),
        vec![
            at("IOS-002", rel, 8),
            at("IOS-002", rel, 9),
            at("IOS-002", rel, 10),
            at("IOS-002", rel, 11),
            at("IOS-002", rel, 12),
        ]
    );
}

#[test]
fn lookalikes_in_kotlin_are_not_findings() {
    let dir = "PhoneConnect/composeUi/src/iosMain/kotlin/p";
    let cases: [(&str, &str); 8] = [
        (
            "comment and log text",
            "package p\nimport platform.Foundation.NSLog\n/* NSUserDefaults */\n// systemUptime\nfun a() = NSLog(\"migrated from NSUserDefaults to Keychain\")\n",
        ),
        (
            "longer identifier",
            "package p\nimport com.russhwolf.settings.NSUserDefaultsSettings\nfun a(d: Any) = NSUserDefaultsSettings::class\n",
        ),
        (
            "imported but never used",
            "package p\nimport platform.Foundation.NSUserDefaults\nimport platform.posix.stat\nfun a() = 1\n",
        ),
        (
            "the project's own function with a C name",
            "package p\nfun stat(x: Int) = x\nfun statfs(x: Int) = x\nfun a() = stat(1) + statfs(2)\n",
        ),
        (
            "struct type without a call",
            "package p\nimport kotlinx.cinterop.*\nimport platform.posix.stat\nfun a() = memScoped { alloc<stat>() }\n",
        ),
        (
            "other attribute key",
            "package p\nimport platform.Foundation.NSFileManager\nfun a(fm: NSFileManager) = fm.attributesOfItemAtPath(\"/x\", null)?.get(\"NSFileSize\")\n",
        ),
        (
            "member of the project's own class",
            "package p\nclass Clock { val systemUptime = 1.0 }\nfun a() = Clock().systemUptime\n",
        ),
        (
            "the project's own class with an Apple name",
            "package p\nclass NSUserDefaults { fun get() = 1 }\nfun a() = NSUserDefaults().get()\n",
        ),
    ];
    for (what, body) in cases {
        assert_clean(what, &[(&format!("{dir}/Case.kt"), body)]);
    }
}

#[test]
fn kotlin_outside_an_apple_main_source_set_is_not_scanned() {
    for set in ["commonMain", "androidMain", "iosTest", "jvmMain"] {
        let rel = format!("PhoneConnect/composeUi/src/{set}/kotlin/p/Prefs.kt");
        assert_clean(set, &[(&rel, PREFS_KT)]);
    }
}

#[test]
fn module_that_no_xcode_project_links_is_unresolved_not_critical() {
    // `shared` is its own Gradle build: nothing in the repository says which
    // app bundles it, or with which manifest.
    let rel = "shared/src/iosMain/kotlin/q/Trust.kt";
    let report = run(&[(rel, PREFS_KT)]);
    assert_eq!(report.status, StoreStatus::Clear);
    assert_eq!(found(&report, Severity::Critical), vec![]);
    assert_eq!(found(&report, Severity::Warn), vec![at("IOS-002", rel, 6)]);
    assert_eq!(report.unresolved.len(), 1, "{:#?}", report.unresolved);
    assert!(report.unresolved[0].starts_with("IOS-002 shared/src/iosMain"));
}
