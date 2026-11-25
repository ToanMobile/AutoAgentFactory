//! Stack detection by marker files, for `aaf init` and for `profile = "auto"`.
mod detect_scripts;

use std::fs;
use std::path::Path;

use aaf_domain::config::{CommandsConfig, RepoConfig};
use detect_scripts::{
    android_gradle_commands, depends_on, package_json_commands, product_flavors_file,
    pyproject_commands,
};

use super::detect_unity;

/// Every concrete value of `project.profile` (besides `auto`).
pub const PROFILES: [&str; 9] = [
    "android",
    "ios",
    "flutter",
    "react-native",
    "expo",
    "web",
    "backend",
    "rust",
    "unity",
];

/// Marker files are small; anything larger is not worth parsing for a hint.
const MAX_MARKER_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    /// One of [`PROFILES`], or `auto` when no marker was found.
    pub profile: String,
    /// Only commands that are certain for the stack. Empty otherwise: an
    /// empty test command makes the gate report `Unverified`, which is more
    /// honest than a guessed command that fails or tests nothing.
    pub commands: CommandsConfig,
    /// Repo-relative markers that led to `profile`.
    pub evidence: Vec<String>,
}

fn found(profile: &str, commands: CommandsConfig, evidence: Vec<String>) -> Detected {
    Detected {
        profile: profile.to_string(),
        commands,
        evidence,
    }
}

fn commands(build: &str, test_scoped: &str, test_full: &str, lint: &str) -> CommandsConfig {
    CommandsConfig {
        build: build.to_string(),
        test_scoped: test_scoped.to_string(),
        test_full: test_full.to_string(),
        lint: lint.to_string(),
        lint_full: String::new(),
        // A repro command is specific to the reported failure; never guessed.
        repro: String::new(),
    }
}

/// Detection is a hint: a marker that is missing, unreadable, oversized or
/// not UTF-8 simply does not count, so every read error maps to `None`.
pub(super) fn read_marker(path: &Path) -> Option<String> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_MARKER_BYTES {
        return None;
    }
    fs::read_to_string(path).ok()
}

pub(super) fn read_json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_str(&read_marker(path)?).ok()
}

/// Sorted names of the entries directly under `root`, for stable results.
fn child_names(root: &Path) -> Vec<String> {
    // An unreadable root has no markers to offer; same reasoning as `read_marker`.
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

fn android_manifest(root: &Path, children: &[String]) -> Option<String> {
    let in_module = |module: &str| format!("{module}/src/main/AndroidManifest.xml");
    let mut candidates = vec![in_module("app"), "src/main/AndroidManifest.xml".to_string()];
    candidates.extend(
        children
            .iter()
            .filter(|name| !name.starts_with('.'))
            .map(|name| in_module(name)),
    );
    candidates.into_iter().find(|rel| root.join(rel).is_file())
}

/// Cross-platform frameworks are checked before the native shells they
/// generate (`android/`, `ios/`), and specific stacks before the generic
/// `package.json` fallback.
pub fn detect_stack(repo_root: &Path) -> Detected {
    let is_file = |rel: &str| repo_root.join(rel).is_file();
    let none = CommandsConfig::default;

    if is_file(detect_unity::VERSION_FILE) {
        // EditMode tests are the one run that is certain for a Unity
        // project; a player build or PlayMode tests depend on the project.
        let (test, why) = detect_unity::test_command(repo_root);
        return found(
            "unity",
            commands("", &test, &test, ""),
            vec![detect_unity::VERSION_FILE.to_string(), why],
        );
    }

    if let Some(pubspec) = read_marker(&repo_root.join("pubspec.yaml"))
        && pubspec.contains("sdk: flutter")
    {
        if repo_root.join("test").is_dir() {
            return found(
                "flutter",
                commands("", "flutter test", "flutter test", "flutter analyze"),
                vec!["pubspec.yaml".to_string(), "test/".to_string()],
            );
        }
        return found(
            "flutter",
            commands("", "", "", "flutter analyze"),
            vec!["pubspec.yaml (no test/ directory)".to_string()],
        );
    }

    let package = read_json(&repo_root.join("package.json"));
    let pkg_cmds = || {
        package
            .as_ref()
            .map(|p| package_json_commands(repo_root, p))
            .unwrap_or_default()
    };

    let app_json = read_json(&repo_root.join("app.json"));
    if app_json.is_some_and(|app| app.get("expo").is_some()) {
        return found("expo", pkg_cmds(), vec!["app.json (\"expo\")".to_string()]);
    }
    let pkg_has_expo = package.as_ref().is_some_and(|p| depends_on(p, "expo"));
    if let Some(cfg) = [
        "app.config.ts",
        "app.config.js",
        "app.config.mjs",
        "app.config.cjs",
    ]
    .into_iter()
    .find(|rel| {
        is_file(rel)
            && (pkg_has_expo
                || read_marker(&repo_root.join(rel)).is_some_and(|text| {
                    text.contains("\"expo\"")
                        || text.contains("'expo'")
                        || text.contains("\"expo/")
                        || text.contains("'expo/")
                }))
    }) {
        return found("expo", pkg_cmds(), vec![cfg.to_string()]);
    }

    if pkg_has_expo {
        return found("expo", pkg_cmds(), vec!["package.json (expo)".to_string()]);
    }
    if package
        .as_ref()
        .is_some_and(|p| depends_on(p, "react-native"))
    {
        return found(
            "react-native",
            pkg_cmds(),
            vec!["package.json (react-native)".to_string()],
        );
    }

    let children = child_names(repo_root);

    let gradle = ["build.gradle.kts", "build.gradle"]
        .into_iter()
        .find(|rel| is_file(rel));
    if let Some(gradle) = gradle
        && let Some(manifest) = android_manifest(repo_root, &children)
    {
        let mut evidence = vec![gradle.to_string(), manifest];
        // Without the wrapper the Gradle version is whatever is on PATH, if any.
        let cmds = if !is_file("gradlew") {
            none()
        } else if let Some(flavored) = product_flavors_file(repo_root, &children) {
            evidence.push("gradlew".to_string());
            // Flavors rename the variant tasks (`test<Flavor>DebugUnitTest`);
            // the plain `testDebugUnitTest` does not exist then.
            evidence.push(format!(
                "{flavored} (productFlavors: variant task names are project-specific, commands left empty)"
            ));
            none()
        } else {
            evidence.push("gradlew".to_string());
            android_gradle_commands("")
        };
        return found("android", cmds, evidence);
    }

    if let Some(project) = children
        .iter()
        .find(|name| name.ends_with(".xcworkspace") && repo_root.join(name).is_dir())
        .or_else(|| {
            children
                .iter()
                .find(|name| name.ends_with(".xcodeproj") && repo_root.join(name).is_dir())
        })
    {
        // Scheme and destination are project-specific, so no xcodebuild line is certain.
        return found("ios", none(), vec![project.clone()]);
    }
    if is_file("Package.swift") {
        return found(
            "ios",
            commands("swift build", "swift test", "swift test", ""),
            vec!["Package.swift".to_string()],
        );
    }

    if is_file("Cargo.toml") {
        return found(
            "rust",
            commands(
                "cargo build",
                "cargo test",
                "cargo test --workspace",
                // Every member and its test targets, like the test command.
                "cargo clippy --workspace --all-targets -- -D warnings",
            ),
            vec!["Cargo.toml".to_string()],
        );
    }

    if is_file("go.mod") {
        return found(
            "backend",
            commands(
                "go build ./...",
                "go test ./...",
                "go test ./...",
                "go vet ./...",
            ),
            vec!["go.mod".to_string()],
        );
    }
    if is_file("pyproject.toml") {
        return found(
            "backend",
            pyproject_commands(repo_root),
            vec!["pyproject.toml".to_string()],
        );
    }

    if let Some(pkg) = &package {
        return found(
            "web",
            package_json_commands(repo_root, pkg),
            vec!["package.json".to_string()],
        );
    }

    if let Some(nested) = nested_gradle_builds(repo_root, &children) {
        return nested;
    }

    found("auto", none(), Vec::new())
}

/// A name that is one shell word and one path component as it stands.
fn is_plain_name(name: &str) -> bool {
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !name.starts_with('-')
}

/// A monorepo whose Android builds live one directory down (each child with
/// its own `settings.gradle(.kts)` and an Android manifest), with nothing at
/// the root that names a stack.
fn nested_gradle_builds(repo_root: &Path, children: &[String]) -> Option<Detected> {
    let mut builds: Vec<(&String, String)> = Vec::new();
    for dir in children.iter().filter(|name| !name.starts_with('.')) {
        let build_root = repo_root.join(dir);
        let Some(settings) = ["settings.gradle.kts", "settings.gradle"]
            .into_iter()
            .find(|file| build_root.join(file).is_file())
        else {
            continue;
        };
        if android_manifest(&build_root, &child_names(&build_root)).is_some() {
            builds.push((dir, format!("{dir}/{settings}")));
        }
    }
    let mut evidence: Vec<String> = builds
        .iter()
        .map(|(_, settings)| settings.clone())
        .collect();
    let (dir, _) = builds.first()?;
    if builds.len() > 1 {
        evidence.push(format!(
            "{} Gradle builds below the root: which tasks to run in which build is \
             project-specific, commands left empty",
            builds.len()
        ));
        return Some(found("android", CommandsConfig::default(), evidence));
    }
    let build_root = repo_root.join(dir);
    let cmds = if !build_root.join("gradlew").is_file() || !is_plain_name(dir) {
        CommandsConfig::default()
    } else if let Some(flavored) = product_flavors_file(&build_root, &child_names(&build_root)) {
        evidence.push(format!(
            "{dir}/{flavored} (productFlavors: variant task names are project-specific, commands left empty)"
        ));
        CommandsConfig::default()
    } else {
        evidence.push(format!("{dir}/gradlew"));
        android_gradle_commands(&format!("cd {dir} && "))
    };
    Some(found("android", cmds, evidence))
}

/// The configured profile, with `auto` resolved by [`detect_stack`]. Stays
/// `auto` when nothing is detected.
pub fn effective_profile(repo: &RepoConfig, repo_root: &Path) -> String {
    if repo.project.profile == "auto" {
        detect_stack(repo_root).profile
    } else {
        repo.project.profile.clone()
    }
}

/// Sensible default `[proof].trigger_globs` for UI-bearing project stacks.
///
/// Returns an empty list for headless/non-UI stacks (`rust`, `backend`, `auto`, ...).
pub fn default_proof_trigger_globs(profile: &str) -> Vec<String> {
    let globs: &[&str] = match profile.trim().to_ascii_lowercase().as_str() {
        "android" => &["res/layout/**", "**/res/layout/**", "**/ui/**", "**/*.kt"],
        "ios" => &["**/*.swift", "**/*.storyboard"],
        "flutter" => &["lib/**"],
        "react-native" | "expo" => &["src/**", "app/**"],
        "web" => &["src/**", "app/**", "pages/**"],
        _ => &[],
    };
    globs.iter().map(|s| (*s).to_string()).collect()
}
