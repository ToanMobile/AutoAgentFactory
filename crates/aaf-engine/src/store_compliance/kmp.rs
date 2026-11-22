//! Required Reason API use in Kotlin Multiplatform code that is compiled for
//! Apple platforms (`src/iosMain/**/*.kt` and its siblings).
//!
//! Kotlin/Native reaches the Apple SDK through the `platform.*` packages
//! under the Objective-C names, so a KMP app can use every one of these APIs
//! without a line of Swift. The list is Apple's, per privacy category:
//! <https://developer.apple.com/documentation/bundleresources/app-privacy-configuration/nsprivacyaccessedapitypes/nsprivacyaccessedapitype>
//! (read 2026-11-22; Objective-C names as that page gives them). Overview:
//! <https://developer.apple.com/documentation/bundleresources/describing-use-of-required-reason-api>.
//!
//! `getattrlist`, `fgetattrlist` and `getattrlistat` are on Apple's list in
//! two categories (file timestamp and disk space). Which one applies depends
//! on the attributes asked for, which is not readable from a call site, so
//! they are left out instead of being pinned on one category.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::text::strip_comments;

const USER_DEFAULTS: &str = "NSPrivacyAccessedAPICategoryUserDefaults";
const FILE_TIMESTAMP: &str = "NSPrivacyAccessedAPICategoryFileTimestamp";
const BOOT_TIME: &str = "NSPrivacyAccessedAPICategorySystemBootTime";
const DISK_SPACE: &str = "NSPrivacyAccessedAPICategoryDiskSpace";
const KEYBOARDS: &str = "NSPrivacyAccessedAPICategoryActiveKeyboards";

/// Source sets the Kotlin Gradle plugin compiles for iOS (the default
/// hierarchy template). Test source sets are not shipped and not listed.
const APPLE_SOURCE_SETS: [&str; 5] = [
    "iosMain",
    "appleMain",
    "iosArm64Main",
    "iosX64Main",
    "iosSimulatorArm64Main",
];

pub(crate) fn is_apple_source_set(name: &str) -> bool {
    APPLE_SOURCE_SETS.contains(&name)
}

/// True for a path below `src/<apple source set>/`.
pub(crate) fn in_apple_source_set(dir: &Path) -> bool {
    module_dir(dir).is_some()
}

/// The Gradle module a Kotlin file belongs to: the directory that holds
/// `src/<apple source set>`.
pub(crate) fn module_dir(rel: &Path) -> Option<PathBuf> {
    let parts: Vec<_> = rel.components().collect();
    let at = parts.windows(2).position(|pair| {
        pair[0].as_os_str() == "src" && is_apple_source_set(&pair[1].as_os_str().to_string_lossy())
    })?;
    Some(parts[..at].iter().collect())
}

/// Where a top-level symbol comes from.
#[derive(Clone, Copy, PartialEq)]
enum Origin {
    /// A class or constant of `platform.Foundation`.
    Foundation,
    /// A C function of `platform.posix` or `platform.darwin`; only a call
    /// counts (`stat` is also the name of the struct).
    C,
}

/// Top-level symbols. One counts when the file imports it from `platform.*`
/// (by name or with the package wildcard) or names it fully qualified: the
/// bare word could be the project's own declaration.
const SYMBOLS: [(&str, Origin, &str); 21] = [
    ("NSUserDefaults", Origin::Foundation, USER_DEFAULTS),
    ("NSFileCreationDate", Origin::Foundation, FILE_TIMESTAMP),
    ("NSFileModificationDate", Origin::Foundation, FILE_TIMESTAMP),
    (
        "NSURLContentModificationDateKey",
        Origin::Foundation,
        FILE_TIMESTAMP,
    ),
    ("NSURLCreationDateKey", Origin::Foundation, FILE_TIMESTAMP),
    ("getattrlistbulk", Origin::C, FILE_TIMESTAMP),
    ("stat", Origin::C, FILE_TIMESTAMP),
    ("fstat", Origin::C, FILE_TIMESTAMP),
    ("fstatat", Origin::C, FILE_TIMESTAMP),
    ("lstat", Origin::C, FILE_TIMESTAMP),
    ("mach_absolute_time", Origin::C, BOOT_TIME),
    (
        "NSURLVolumeAvailableCapacityKey",
        Origin::Foundation,
        DISK_SPACE,
    ),
    (
        "NSURLVolumeAvailableCapacityForImportantUsageKey",
        Origin::Foundation,
        DISK_SPACE,
    ),
    (
        "NSURLVolumeAvailableCapacityForOpportunisticUsageKey",
        Origin::Foundation,
        DISK_SPACE,
    ),
    (
        "NSURLVolumeTotalCapacityKey",
        Origin::Foundation,
        DISK_SPACE,
    ),
    ("NSFileSystemFreeSize", Origin::Foundation, DISK_SPACE),
    ("NSFileSystemSize", Origin::Foundation, DISK_SPACE),
    ("statfs", Origin::C, DISK_SPACE),
    ("statvfs", Origin::C, DISK_SPACE),
    ("fstatfs", Origin::C, DISK_SPACE),
    ("fstatvfs", Origin::C, DISK_SPACE),
];

/// Members reached through an object (`NSProcessInfo.processInfo.systemUptime`):
/// they are not imported, so the class they belong to must be named in the
/// file's code.
const MEMBERS: [(&str, &str, &str); 3] = [
    ("systemUptime", "NSProcessInfo", BOOT_TIME),
    ("activeInputModes", "UITextInputMode", KEYBOARDS),
    ("fileModificationDate", "UIDocument", FILE_TIMESTAMP),
];

/// File attribute keys are `NSString` constants whose value is their own
/// name, and Kotlin code often passes the string itself:
/// `attributesOfItemAtPath(path, null)?.get("NSFileModificationDate")`. A
/// string literal that is exactly the key is the same access.
const LITERAL_KEYS: [(&str, &str); 4] = [
    ("NSFileCreationDate", FILE_TIMESTAMP),
    ("NSFileModificationDate", FILE_TIMESTAMP),
    ("NSFileSystemFreeSize", DISK_SPACE),
    ("NSFileSystemSize", DISK_SPACE),
];

/// One use: 0-based line, the privacy manifest category, the API name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Use {
    pub line: usize,
    pub category: &'static str,
    pub api: &'static str,
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// What the file imports from `platform.*`: full names and wildcard packages.
struct Imports {
    names: BTreeSet<String>,
    any: bool,
}

impl Imports {
    fn of(code: &str) -> Imports {
        let names: BTreeSet<String> = code
            .lines()
            .filter_map(|line| line.trim().strip_prefix("import "))
            // An aliased import renames the symbol; the alias is not followed.
            .filter(|path| !path.contains(" as "))
            .map(|path| path.trim().trim_end_matches(';').to_string())
            .filter(|path| path.starts_with("platform."))
            .collect();
        Imports {
            any: !names.is_empty(),
            names,
        }
    }

    fn brings(&self, name: &str, origin: Origin) -> bool {
        let packages: &[&str] = match origin {
            Origin::Foundation => &["platform.Foundation"],
            Origin::C => &["platform.posix", "platform.darwin"],
        };
        packages.iter().any(|package| {
            self.names.contains(&format!("{package}.{name}"))
                || self.names.contains(&format!("{package}.*"))
        })
    }
}

/// `text` ends with `platform.<Package>.`: the next identifier is qualified.
fn qualified(before: &str) -> bool {
    let Some(rest) = before.strip_suffix('.') else {
        return false;
    };
    let package_at = rest
        .rfind(|c: char| !is_ident_char(c))
        .map_or(0, |at| at + 1);
    !rest[package_at..].is_empty() && rest[..package_at].ends_with("platform.")
}

/// Required Reason API uses in one Kotlin source of an Apple source set.
pub(crate) fn uses(source: &str) -> Vec<Use> {
    // Comments go, strings stay: a literal attribute key is an access.
    let code = strip_comments(source, true);
    let imports = Imports::of(&code);
    let names_class = |class: &str| {
        code.split(|c: char| !is_ident_char(c))
            .any(|word| word == class)
    };
    let mut found = Vec::new();
    for (line, text) in code.lines().enumerate() {
        if text.trim_start().starts_with("import ") {
            // Importing a symbol is not using it.
            continue;
        }
        let mut hit = |category: &'static str, api: &'static str| {
            found.push(Use {
                line,
                category,
                api,
            });
        };
        let mut in_string = false;
        let mut start = 0usize;
        let mut chars = text.char_indices().peekable();
        while let Some((at, c)) = chars.next() {
            if c == '\\' && in_string {
                chars.next();
                continue;
            }
            if c == '"' {
                if in_string
                    && let Some((key, category)) = LITERAL_KEYS
                        .iter()
                        .find(|(key, _)| *key == &text[start..at])
                    && imports.any
                {
                    hit(category, key);
                }
                in_string = !in_string;
                start = at + 1;
                continue;
            }
            if in_string || !is_ident_char(c) {
                continue;
            }
            // An identifier in code: take all of it.
            let mut end = at + c.len_utf8();
            while let Some((next, n)) = chars.peek().copied() {
                if !is_ident_char(n) {
                    break;
                }
                end = next + n.len_utf8();
                chars.next();
            }
            let (word, before, after) = (&text[at..end], &text[..at], text[end..].trim_start());
            if let Some((name, origin, category)) = SYMBOLS.iter().find(|(name, ..)| *name == word)
            {
                let is_call = *origin != Origin::C || after.starts_with('(');
                if is_call && (imports.brings(name, *origin) || qualified(before)) {
                    hit(category, name);
                }
            } else if let Some((name, class, category)) =
                MEMBERS.iter().find(|(name, ..)| *name == word)
                && before.trim_end().ends_with('.')
                && names_class(class)
            {
                hit(category, name);
            }
        }
    }
    found
}
