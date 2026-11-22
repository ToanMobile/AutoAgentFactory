//! IOS-001 / IOS-102: purpose strings (`NS*UsageDescription`) from
//! `Info.plist` and from `INFOPLIST_KEY_*` build settings in `project.pbxproj`.
//! The Required Reason API rules (IOS-002 / IOS-101) live in `privacy.rs`.

use std::collections::BTreeSet;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use aaf_domain::gate::Severity;

use super::detect::Inventory;
use super::report::{IOS_001, IOS_102, Sink};
use super::text::{
    decode_strings_file, line_number, line_of_needle, read_bytes, read_text, rel_str,
};

/// Whole values that are obviously not a purpose string.
const PLACEHOLDER_VALUES: [&str; 16] = [
    "todo",
    "tbd",
    "tba",
    "test",
    "testing",
    "placeholder",
    "description",
    "usage description",
    "fixme",
    "lorem ipsum",
    "changeme",
    "change me",
    "n/a",
    "asdf",
    "foo",
    "string",
];

/// Case-sensitive markers that open an unfinished value. Upper case only:
/// "Todo lists can attach photos" is a legitimate sentence.
const PLACEHOLDER_PREFIXES: [&str; 3] = ["TODO", "TBD", "FIXME"];

const MIN_PURPOSE_CHARS: usize = 10;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Purpose {
    Empty,
    Placeholder,
    /// Contains a build setting reference and too little literal text to judge.
    Variable,
    TooShort,
    Ok,
}

fn starts_with_word(value: &str, word: &str) -> bool {
    value
        .strip_prefix(word)
        .is_some_and(|rest| !rest.starts_with(|c: char| c.is_alphanumeric()))
}

/// Remove `$(NAME)` and `${NAME}` references.
fn without_variables(value: &str) -> (String, bool) {
    let mut out = String::new();
    let mut rest = value;
    let mut had = false;
    while let Some(at) = rest.find("$(").or_else(|| rest.find("${")) {
        let close = if rest[at..].starts_with("$(") {
            ')'
        } else {
            '}'
        };
        out.push_str(&rest[..at]);
        had = true;
        match rest[at..].find(close) {
            Some(end) => rest = &rest[at + end + 1..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    (out, had)
}

/// Decide whether a purpose string is real. The placeholder test is on the
/// whole value, so a sentence that merely contains "test" is accepted.
pub(crate) fn judge(value: &str) -> Purpose {
    let (literal, has_variable) = without_variables(value);
    let trimmed = literal.trim();
    if has_variable {
        return if trimmed.chars().count() >= MIN_PURPOSE_CHARS {
            Purpose::Ok
        } else {
            Purpose::Variable
        };
    }
    if trimmed.is_empty() {
        return Purpose::Empty;
    }
    let core = trimmed
        .trim_matches(|c: char| !c.is_alphanumeric() && c != '/')
        .to_lowercase();
    let only_x = core.len() >= 2 && core.chars().all(|c| c == 'x');
    if core.is_empty()
        || only_x
        || PLACEHOLDER_VALUES.contains(&core.as_str())
        || PLACEHOLDER_PREFIXES
            .iter()
            .any(|p| starts_with_word(trimmed, p))
        || starts_with_word(&trimmed.to_lowercase(), "lorem ipsum")
    {
        return Purpose::Placeholder;
    }
    if trimmed.chars().count() < MIN_PURPOSE_CHARS {
        return Purpose::TooShort;
    }
    Purpose::Ok
}

pub(crate) fn is_usage_key(key: &str) -> bool {
    key.starts_with("NS") && key.ends_with("UsageDescription")
}

/// Report one purpose string. `certain` is false when the value may be
/// replaced before it reaches the store (unreferenced plist, dynamic config).
#[allow(clippy::too_many_arguments)]
pub(crate) fn report_purpose(
    sink: &mut Sink,
    file: &str,
    line: Option<u32>,
    key: &str,
    value: &str,
    certain: bool,
    uncertainty: &str,
) {
    let fix = "Write one sentence that tells the user why the app needs this access and what it does with it.";
    match judge(value) {
        Purpose::Ok => {}
        Purpose::Empty => sink.critical_if(
            certain,
            uncertainty,
            &IOS_001,
            file,
            line,
            format!("{key} is empty, so App Review has no purpose string to show the user."),
            fix,
        ),
        Purpose::Placeholder => sink.critical_if(
            certain,
            uncertainty,
            &IOS_001,
            file,
            line,
            format!(
                "{key} is the placeholder `{}` instead of a purpose string.",
                value.trim()
            ),
            fix,
        ),
        Purpose::Variable => sink.unresolved(
            &IOS_001,
            file,
            line,
            format!(
                "{key} is `{}`, a build setting that is only known at build time.",
                value.trim()
            ),
            "Check the resolved purpose string in the built Info.plist.",
        ),
        Purpose::TooShort => sink.finding(
            &IOS_102,
            Severity::Warn,
            file,
            line,
            format!(
                "{key} is only {} characters long, too short to explain the use.",
                value.trim().chars().count()
            ),
            fix,
        ),
    }
}

/// An iOS (or iPadOS/tvOS/visionOS-style) project; a macOS-only project has
/// different submission rules and is not judged here.
pub(crate) fn is_ios_project(pbxproj: &str) -> bool {
    pbxproj.contains("iphoneos") || pbxproj.contains("IPHONEOS_DEPLOYMENT_TARGET")
}

/// Directory that holds `Foo.xcodeproj` (Xcode's `SRCROOT`).
pub(crate) fn project_root(pbxproj_rel: &Path) -> PathBuf {
    pbxproj_rel
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

/// `KEY = value;` of a pbxproj build-settings line, unquoted and unescaped.
fn build_setting(line: &str) -> Option<(&str, String)> {
    let (key, rest) = line.trim().split_once('=')?;
    let raw = rest.trim().strip_suffix(';')?.trim();
    let value = match raw.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        Some(quoted) => quoted
            .replace("\\\"", "\"")
            .replace("\\n", "\n")
            .replace("\\\\", "\\"),
        None => raw.to_string(),
    };
    Some((key.trim(), value))
}

/// Names of keys that a localisation file overrides for some language.
fn localized_keys(root: &Path, inv: &Inventory) -> String {
    inv.infoplist_strings
        .iter()
        // An unreadable localisation file overrides nothing we can prove.
        .filter_map(|p| read_bytes(&root.join(p)).ok())
        .map(|b| decode_strings_file(&b))
        .collect::<Vec<_>>()
        .join("\n")
}

struct PlistJob {
    rel: PathBuf,
    referenced: bool,
}

pub(crate) fn scan(root: &Path, inv: &Inventory, sink: &mut Sink) {
    let localized = localized_keys(root, inv);
    let mut referenced: BTreeSet<PathBuf> = BTreeSet::new();
    let mut seen_settings: BTreeSet<(String, String, String)> = BTreeSet::new();
    for rel in &inv.pbxprojs {
        let file = rel_str(rel);
        let text = match read_text(&root.join(rel)) {
            Ok(t) => t,
            Err(e) => {
                sink.unreadable(&file, &e);
                continue;
            }
        };
        if !is_ios_project(&text) {
            continue;
        }
        let src_root = project_root(rel);
        for (index, line) in text.lines().enumerate() {
            let Some((key, value)) = build_setting(line) else {
                continue;
            };
            let line_no = line_number(index);
            if key == "INFOPLIST_FILE" {
                let path = [
                    "$(SRCROOT)/",
                    "${SRCROOT}/",
                    "$(PROJECT_DIR)/",
                    "${PROJECT_DIR}/",
                ]
                .iter()
                .find_map(|p| value.strip_prefix(p))
                .unwrap_or(&value);
                // A path that leaves the project directory is not followed:
                // the scanner only reads inside the repository.
                if path.contains("$(") || path.contains("${") || path.split('/').any(|p| p == "..")
                {
                    if seen_settings.insert((file.clone(), key.to_string(), value.clone())) {
                        sink.unresolved(
                            &IOS_001,
                            &file,
                            line_no,
                            format!("INFOPLIST_FILE is `{value}`, a path the scanner cannot follow statically."),
                            "Check the purpose strings of the Info.plist this setting resolves to.",
                        );
                    }
                } else if !path.is_empty() {
                    referenced.insert(src_root.join(path));
                }
            } else if let Some(usage_key) = key.strip_prefix("INFOPLIST_KEY_")
                && is_usage_key(usage_key)
                // Debug and Release usually repeat the same setting.
                && seen_settings.insert((file.clone(), key.to_string(), value.clone()))
            {
                judge_localized(
                    sink, &localized, &file, line_no, usage_key, &value, true, "",
                );
            }
        }
    }
    let mut jobs: Vec<PlistJob> = referenced
        .iter()
        .map(|rel| PlistJob {
            rel: rel.clone(),
            referenced: true,
        })
        .collect();
    jobs.extend(
        inv.info_plists
            .iter()
            .filter(|p| !referenced.contains(*p))
            .map(|rel| PlistJob {
                rel: rel.clone(),
                referenced: false,
            }),
    );
    for job in jobs {
        scan_plist(root, &job, &localized, sink);
    }
}

/// An empty or placeholder value may be replaced per language by
/// `InfoPlist.strings`; then the shipped string is not known statically.
#[allow(clippy::too_many_arguments)]
fn judge_localized(
    sink: &mut Sink,
    localized: &str,
    file: &str,
    line: Option<u32>,
    key: &str,
    value: &str,
    certain: bool,
    uncertainty: &str,
) {
    let bad = matches!(judge(value), Purpose::Empty | Purpose::Placeholder);
    if bad && localized.contains(key) {
        sink.unresolved(
            &IOS_001,
            file,
            line,
            format!("{key} is `{}` here but InfoPlist.strings localises it.", value.trim()),
            "Give the base value a real purpose string as well; it is the fallback for unlisted languages.",
        );
        return;
    }
    report_purpose(sink, file, line, key, value, certain, uncertainty);
}

fn scan_plist(root: &Path, job: &PlistJob, localized: &str, sink: &mut Sink) {
    let file = rel_str(&job.rel);
    let path = root.join(&job.rel);
    if job.referenced && !path.is_file() {
        sink.unresolved(
            &IOS_001,
            &file,
            None,
            "The Xcode project names this Info.plist but the file does not exist in the repository.".to_string(),
            "Commit the Info.plist or fix INFOPLIST_FILE.",
        );
        return;
    }
    let bytes = match read_bytes(&path) {
        Ok(b) => b,
        Err(e) => {
            sink.unreadable(&file, &e);
            return;
        }
    };
    let dict = match plist::Value::from_reader(Cursor::new(&bytes)) {
        Ok(plist::Value::Dictionary(d)) => d,
        Ok(_) => {
            sink.unreadable(&file, "the property list root is not a dictionary");
            return;
        }
        Err(e) => {
            sink.unreadable(&file, &format!("invalid property list ({e})"));
            return;
        }
    };
    // Binary plists have no lines; XML ones are searched for the key element.
    let text = String::from_utf8_lossy(&bytes);
    for (key, value) in dict.iter().filter(|(k, _)| is_usage_key(k)) {
        let line = line_of_needle(&text, &format!("<key>{key}</key>"));
        let Some(value) = value.as_string() else {
            sink.unresolved(
                &IOS_001,
                &file,
                line,
                format!("{key} is not a string value."),
                "Make the purpose string a <string> element.",
            );
            continue;
        };
        judge_localized(
            sink,
            localized,
            &file,
            line,
            key,
            value,
            job.referenced,
            "no Xcode project in the repository references this Info.plist",
        );
    }
}
