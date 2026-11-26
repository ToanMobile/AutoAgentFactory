//! IOS-002 / IOS-101: Required Reason API use versus `PrivacyInfo.xcprivacy`.
//!
//! Source: Apple, "Describing use of required reason API". Only app sources
//! are searched (Swift, Objective-C, and the Kotlin of a Kotlin Multiplatform
//! module's iOS source sets, see [`super::kmp`]); `Pods/`, `Carthage/` and
//! build output are excluded by the walk because third-party SDKs ship their
//! own manifest.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Cursor;
use std::path::{Path, PathBuf};

use aaf_domain::gate::Severity;

use super::detect::Inventory;
use super::ios::{is_ios_project, project_root};
use super::kmp;
use super::report::{IOS_002, IOS_101, Sink};
use super::text::{line_number, read_bytes, read_text, read_text_lossy, rel_str, strip_comments};

struct Category {
    /// Value of `NSPrivacyAccessedAPIType` in the privacy manifest.
    manifest_type: &'static str,
    label: &'static str,
    /// API names; a trailing `*` also accepts longer identifiers.
    tokens: &'static [&'static str],
}

const CATEGORIES: [Category; 5] = [
    Category {
        manifest_type: "NSPrivacyAccessedAPICategoryUserDefaults",
        label: "user defaults",
        tokens: &["UserDefaults", "NSUserDefaults"],
    },
    Category {
        manifest_type: "NSPrivacyAccessedAPICategoryFileTimestamp",
        label: "file timestamp",
        tokens: &[
            "fileModificationDate",
            "contentModificationDateKey",
            "creationDateKey",
        ],
    },
    Category {
        manifest_type: "NSPrivacyAccessedAPICategorySystemBootTime",
        label: "system boot time",
        tokens: &["systemUptime", "mach_absolute_time"],
    },
    Category {
        manifest_type: "NSPrivacyAccessedAPICategoryDiskSpace",
        label: "disk space",
        tokens: &["volumeAvailableCapacity*", "systemFreeSize"],
    },
    Category {
        manifest_type: "NSPrivacyAccessedAPICategoryActiveKeyboards",
        label: "active keyboards",
        tokens: &["activeInputModes"],
    },
];

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Index into `CATEGORIES` of the first required-reason API named on `line`,
/// matched as a whole identifier.
fn category_on_line(line: &str) -> Option<usize> {
    let mut start = None;
    let mut found = None;
    let mut check = |word: &str, rest: &str| {
        // `UserDefaults.didChangeNotification` names a notification; it does
        // not read or write user defaults.
        let notification_only =
            word.ends_with("UserDefaults") && rest.starts_with(".didChangeNotification");
        if found.is_none() && !notification_only {
            found = CATEGORIES.iter().position(|c| {
                c.tokens.iter().any(|t| match t.strip_suffix('*') {
                    Some(prefix) => word.starts_with(prefix),
                    None => word == *t,
                })
            });
        }
    };
    for (i, c) in line.char_indices() {
        match (is_ident_char(c), start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                check(&line[s..i], &line[i..]);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        check(&line[s..], "");
    }
    found
}

/// Unit and UI test targets are never part of the uploaded app.
pub(super) fn in_test_dir(rel: &Path) -> bool {
    rel.components().any(|c| {
        let c = c.as_os_str().to_string_lossy();
        c == "Tests" || c == "Test" || c.ends_with("Tests")
    })
}

#[derive(Default)]
struct Usage {
    /// First place the category is used.
    first: Option<(String, Option<u32>)>,
    /// The API named there, when the scanner knows which one it was.
    first_api: Option<&'static str>,
    files: BTreeSet<String>,
}

impl Usage {
    fn add(&mut self, file: &str, index: usize, api: Option<&'static str>) {
        if self.first.is_none() {
            self.first = Some((file.to_string(), line_number(index)));
            self.first_api = api;
        }
        self.files.insert(file.to_string());
    }
}

struct IosProject {
    root: PathBuf,
    /// Text of `project.pbxproj`, to tell which sources the project compiles.
    pbxproj: String,
}

impl IosProject {
    /// True when the Xcode project certainly compiles the file.
    fn compiles(&self, rel: &Path) -> bool {
        // Xcode 16 folder references include every file under the folder.
        if self.pbxproj.contains("PBXFileSystemSynchronizedRootGroup") {
            return true;
        }
        rel.file_name()
            .map(|n| self.pbxproj.contains(&*n.to_string_lossy()))
            .unwrap_or(false)
    }

    /// True when the project certainly links the Kotlin framework of the
    /// Gradle module in `module_dir`: it names the directory the Kotlin
    /// Gradle plugin builds that framework into for Xcode,
    /// `<module>/build/xcode-frameworks`.
    fn links_kotlin_module(&self, module_dir: &Path) -> bool {
        module_dir.file_name().is_some_and(|name| {
            self.pbxproj.contains(&format!(
                "{}/build/xcode-frameworks",
                name.to_string_lossy()
            ))
        })
    }
}

/// Categories declared by one privacy manifest.
fn declared(path: &Path) -> Result<BTreeSet<String>, String> {
    let bytes = read_bytes(path)?;
    let value = plist::Value::from_reader(Cursor::new(&bytes))
        .map_err(|e| format!("invalid property list ({e})"))?;
    let dict = value
        .as_dictionary()
        .ok_or_else(|| "the property list root is not a dictionary".to_string())?;
    let Some(types) = dict.get("NSPrivacyAccessedAPITypes") else {
        return Ok(BTreeSet::new());
    };
    let types = types
        .as_array()
        .ok_or_else(|| "NSPrivacyAccessedAPITypes is not an array".to_string())?;
    Ok(types
        .iter()
        .filter_map(|t| {
            t.as_dictionary()?
                .get("NSPrivacyAccessedAPIType")?
                .as_string()
        })
        .map(str::to_string)
        .collect())
}

pub(crate) fn scan(root: &Path, inv: &Inventory, sink: &mut Sink) {
    let mut projects: Vec<IosProject> = Vec::new();
    // macOS-only projects: the required reason rules do not apply to them.
    let mut other_roots: Vec<PathBuf> = Vec::new();
    for rel in &inv.pbxprojs {
        // An unreadable project file is reported by the purpose-string scan.
        let Ok(pbxproj) = read_text(&root.join(rel)) else {
            continue;
        };
        if is_ios_project(&pbxproj) {
            projects.push(IosProject {
                root: project_root(rel),
                pbxproj,
            });
        } else {
            other_roots.push(project_root(rel));
        }
    }
    // Deepest root first, so a nested project claims its own files.
    projects.sort_by_key(|p| std::cmp::Reverse(p.root.components().count()));

    // Usage per (project index or None for loose files, category index).
    let mut usage: BTreeMap<(Option<usize>, usize), Usage> = BTreeMap::new();
    for rel in &inv.apple_sources {
        if in_test_dir(rel) {
            continue;
        }
        let file = rel_str(rel);
        let text = match read_text_lossy(&root.join(rel)) {
            Ok(t) => strip_comments(&t, false),
            Err(e) => {
                sink.unreadable(&file, &e);
                continue;
            }
        };
        let owner = projects
            .iter()
            .position(|p| rel.starts_with(&p.root) && p.compiles(rel));
        let in_ios_root = projects.iter().any(|p| rel.starts_with(&p.root));
        if !in_ios_root && other_roots.iter().any(|r| rel.starts_with(r)) {
            continue;
        }
        for (index, line) in text.lines().enumerate() {
            if let Some(cat) = category_on_line(line) {
                usage
                    .entry((owner, cat))
                    .or_default()
                    .add(&file, index, None);
            }
        }
    }
    // Gradle modules whose Kotlin framework an iOS project links, with that
    // project: a privacy manifest inside such a module belongs to it.
    let mut kotlin_modules: Vec<(PathBuf, usize)> = Vec::new();
    for rel in &inv.kotlin_apple_sources {
        let Some(module) = kmp::module_dir(rel) else {
            continue;
        };
        let file = rel_str(rel);
        let text = match read_text_lossy(&root.join(rel)) {
            Ok(t) => t,
            Err(e) => {
                sink.unreadable(&file, &e);
                continue;
            }
        };
        let owner = projects.iter().position(|p| p.links_kotlin_module(&module));
        if let Some(owner) = owner
            && !kotlin_modules.iter().any(|(known, _)| *known == module)
        {
            kotlin_modules.push((module, owner));
        }
        for used in kmp::uses(&text) {
            let Some(cat) = CATEGORIES
                .iter()
                .position(|c| c.manifest_type == used.category)
            else {
                continue;
            };
            usage
                .entry((owner, cat))
                .or_default()
                .add(&file, used.line, Some(used.api));
        }
    }
    if usage.is_empty() {
        return;
    }

    // Manifests by owning project (None: outside every iOS project root).
    let mut manifests: BTreeMap<Option<usize>, Vec<&PathBuf>> = BTreeMap::new();
    for rel in &inv.privacy_manifests {
        let owner = projects
            .iter()
            .position(|p| rel.starts_with(&p.root))
            .or_else(|| {
                kotlin_modules
                    .iter()
                    .find(|(module, _)| rel.starts_with(module))
                    .map(|(_, owner)| *owner)
            });
        manifests.entry(owner).or_default().push(rel);
    }
    let mut declared_by: BTreeMap<Option<usize>, Option<BTreeSet<String>>> = BTreeMap::new();
    for (owner, paths) in &manifests {
        let mut all = Some(BTreeSet::new());
        for rel in paths {
            match declared(&root.join(rel)) {
                Ok(set) => {
                    if let Some(a) = all.as_mut() {
                        a.extend(set);
                    }
                }
                Err(e) => {
                    sink.unreadable(&rel_str(rel), &e);
                    // A manifest exists but cannot be read: nothing is asserted
                    // about the categories it may declare.
                    all = None;
                }
            }
        }
        declared_by.insert(*owner, all);
    }

    for ((owner, cat), u) in &usage {
        let category = &CATEGORIES[*cat];
        let Some((file, line)) = u.first.clone() else {
            continue;
        };
        let apis = match u.first_api {
            Some(api) => api.to_string(),
            None => category
                .tokens
                .iter()
                .map(|t| t.trim_end_matches('*'))
                .collect::<Vec<_>>()
                .join(", "),
        };
        let spread = match u.files.len() {
            1 => String::new(),
            n => format!(" (and {} more files)", n - 1),
        };
        // A file outside every project root is compared with the declarations
        // of whichever manifest the repository has.
        let declared = declared_by.get(owner).or_else(|| {
            owner
                .is_none()
                .then(|| declared_by.values().next())
                .flatten()
        });
        match declared {
            None => sink.critical_if(
                owner.is_some(),
                if file.ends_with(".kt") {
                    "no Xcode project in this repository links the Kotlin framework of this module, so the app that bundles it and its manifest are not known"
                } else {
                    "the file is not compiled by an Xcode project in this repository, so the generated project may add its own manifest"
                },
                &IOS_002,
                &file,
                line,
                format!(
                    "App code uses the {} required reason API ({apis}){spread} but the project has no PrivacyInfo.xcprivacy.",
                    category.label
                ),
                &format!(
                    "Add PrivacyInfo.xcprivacy to the app target with an NSPrivacyAccessedAPITypes entry for {} and an approved reason code.",
                    category.manifest_type
                ),
            ),
            // Unreadable manifest: already reported, nothing more to assert.
            Some(None) => {}
            Some(Some(set)) if !set.contains(category.manifest_type) => sink.finding(
                &IOS_101,
                Severity::Warn,
                &file,
                line,
                format!(
                    "App code uses the {} required reason API ({apis}){spread} but PrivacyInfo.xcprivacy does not declare {}.",
                    category.label, category.manifest_type
                ),
                &format!(
                    "Add an NSPrivacyAccessedAPITypes entry for {} with an approved reason code.",
                    category.manifest_type
                ),
            ),
            Some(Some(_)) => {}
        }
    }
}
