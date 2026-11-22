//! Rule catalogue and the collector every platform scanner writes into.

use aaf_domain::gate::{Severity, StoreFinding};

/// One store rule: its stable id, platform and the primary-source guideline it
/// maps to (Apple and Google documentation, in plain words).
pub(crate) struct Rule {
    pub id: &'static str,
    pub platform: &'static str,
    pub guideline: &'static str,
}

/// A file the scanner could not read or parse; never a reason to abort.
pub(crate) const STORE_000: Rule = Rule {
    id: "STORE-000",
    platform: "any",
    guideline: "Scanner integrity: every input file is either checked or reported",
};

/// Play Console refuses to accept a debuggable APK or app bundle
/// (Android developers, "Prepare your app for release": turn off debugging).
pub(crate) const AND_001: Rule = Rule {
    id: "AND-001",
    platform: "android",
    guideline: "Google Play: release builds must not be debuggable (Android docs, Prepare for release)",
};

/// Android 12 behaviour change: a component with an intent filter must set
/// `android:exported`, otherwise an app targeting API 31+ cannot be installed.
pub(crate) const AND_002: Rule = Rule {
    id: "AND-002",
    platform: "android",
    guideline: "Android 12 behavior changes: components with intent filters must declare android:exported",
};

/// Google Play target API level requirement for new apps and updates.
pub(crate) const AND_003: Rule = Rule {
    id: "AND-003",
    platform: "android",
    guideline: "Google Play target API level requirement",
};

/// Android network security configuration: cleartext traffic opt-in.
pub(crate) const AND_101: Rule = Rule {
    id: "AND-101",
    platform: "android",
    guideline: "Android network security configuration: cleartext traffic should be scoped, not global",
};

/// Google Play Permissions policy: restricted permissions need a declaration.
pub(crate) const AND_102: Rule = Rule {
    id: "AND-102",
    platform: "android",
    guideline: "Google Play Permissions policy: restricted permissions require an approved declaration",
};

/// Android Auto Backup: app data is copied to the user's cloud account.
pub(crate) const AND_103: Rule = Rule {
    id: "AND-103",
    platform: "android",
    guideline: "Android Auto Backup guidance: review what allowBackup exposes",
};

/// App Store Review Guideline 5.1.1 and "Requesting access to protected
/// resources": every protected resource needs a real purpose string.
pub(crate) const IOS_001: Rule = Rule {
    id: "IOS-001",
    platform: "ios",
    guideline: "App Store Review Guideline 5.1.1: purpose strings must explain the use of protected resources",
};

/// Apple, "Describing use of required reason API": App Store Connect rejects
/// uploads that use these APIs without a privacy manifest (ITMS-91053).
pub(crate) const IOS_002: Rule = Rule {
    id: "IOS-002",
    platform: "ios",
    guideline: "Apple privacy manifest: required reason API use must be declared in PrivacyInfo.xcprivacy",
};

pub(crate) const IOS_101: Rule = Rule {
    id: "IOS-101",
    platform: "ios",
    guideline: "Apple privacy manifest: each required reason API category in use needs an entry",
};

pub(crate) const IOS_102: Rule = Rule {
    id: "IOS-102",
    platform: "ios",
    guideline: "App Store Review Guideline 5.1.1: purpose strings must be clear and complete",
};

#[derive(Default)]
pub(crate) struct Sink {
    pub findings: Vec<StoreFinding>,
    pub unresolved: Vec<String>,
    /// Set while a cross-platform config (Expo `app.json`) is scanned, so its
    /// findings say where they come from rather than the rule's platform.
    pub platform: Option<&'static str>,
}

/// Platform of a file that could not be read, from its name.
fn platform_of(file: &str) -> &'static str {
    let name = file.rsplit('/').next().unwrap_or(file);
    let ext = name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    if name == "app.json" {
        "expo"
    } else if matches!(
        ext,
        "plist" | "xcprivacy" | "pbxproj" | "swift" | "m" | "mm"
    ) {
        "ios"
    } else if matches!(ext, "xml" | "gradle" | "kts" | "asset") {
        "android"
    } else {
        "any"
    }
}

impl Sink {
    pub fn finding(
        &mut self,
        rule: &Rule,
        severity: Severity,
        file: &str,
        line: Option<u32>,
        message: String,
        fix: &str,
    ) {
        self.findings.push(StoreFinding {
            rule: rule.id.to_string(),
            severity,
            platform: match (self.platform, rule.platform) {
                (_, "any") => platform_of(file).to_string(),
                (Some(p), _) => p.to_string(),
                (None, p) => p.to_string(),
            },
            guideline: rule.guideline.to_string(),
            file: file.to_string(),
            line,
            message,
            fix: fix.to_string(),
        });
    }

    /// Something the scanner could not decide statically: listed in
    /// `unresolved` and surfaced as a warning, never as a Critical and never
    /// dropped.
    pub fn unresolved(
        &mut self,
        rule: &Rule,
        file: &str,
        line: Option<u32>,
        what: String,
        fix: &str,
    ) {
        let location = match line {
            Some(n) => format!("{file}:{n}"),
            None => file.to_string(),
        };
        self.unresolved
            .push(format!("{} {location}: {what}", rule.id));
        self.finding(rule, Severity::Warn, file, line, what, fix);
    }

    /// A Critical when the file certainly ships; otherwise the same
    /// observation as an unresolved warning that says why it is uncertain.
    #[allow(clippy::too_many_arguments)]
    pub fn critical_if(
        &mut self,
        certain: bool,
        uncertainty: &str,
        rule: &Rule,
        file: &str,
        line: Option<u32>,
        message: String,
        fix: &str,
    ) {
        if certain {
            self.finding(rule, Severity::Critical, file, line, message, fix);
        } else {
            self.unresolved(rule, file, line, format!("{message} ({uncertainty})"), fix);
        }
    }

    pub fn unreadable(&mut self, file: &str, error: &str) {
        self.unresolved(
            &STORE_000,
            file,
            None,
            format!("File could not be checked: {error}."),
            "Fix or regenerate the file so it can be parsed, then rerun the scan.",
        );
    }
}
