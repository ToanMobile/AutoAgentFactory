//! AND-003: resolve `targetSdk` of application modules without running Gradle.
//!
//! Only shapes that can be read with certainty are resolved: an integer
//! literal, a version catalog entry, a Gradle property, an `ext`/`val`
//! definition. Anything else is reported as unresolved, never guessed.

use std::path::{Path, PathBuf};

use super::detect::Inventory;
use super::gradle_resolve::{Scope, is_ident_char, resolve};
use super::report::{AND_003, Sink};
use super::text::{line_number, read_text, rel_str, strip_comments};

struct Assignment {
    /// 0-based index of the line.
    index: usize,
    expr: String,
}

#[derive(PartialEq, Eq, Debug)]
enum Kind {
    Application,
    /// Libraries and dynamic features do not decide the app's target SDK.
    Library,
    /// `com.android.test`: produces a test APK only.
    Test,
    Unknown,
}

/// The value expression when `segment` assigns the DSL property `targetSdk`
/// or `targetSdkVersion` (`x = v`, `x v`, `x(v)`), else `None`.
fn assignment_expr(segment: &str) -> Option<&str> {
    let s = segment.trim_start();
    let path_len = s
        .find(|c: char| !(is_ident_char(c) || c == '.'))
        .unwrap_or(s.len());
    let (path, rest) = s.split_at(path_len);
    let (owner, key) = match path.rsplit_once('.') {
        Some((owner, key)) => (owner, key),
        None => ("", path),
    };
    if key != "targetSdk" && key != "targetSdkVersion" {
        return None;
    }
    // `ext.targetSdkVersion = 34` and `libs.versions.targetSdk` define or read
    // a value; they are not the DSL property.
    if owner
        .split('.')
        .any(|p| matches!(p, "ext" | "extra" | "versions" | "libs"))
    {
        return None;
    }
    let after = rest.trim_start();
    let expr = if let Some(v) = after.strip_prefix('=') {
        if v.starts_with('=') {
            return None;
        }
        v
    } else if !owner.is_empty() {
        return None;
    } else if let Some(v) = after.strip_prefix('(') {
        v.rsplit_once(')').map(|(inner, _)| inner)?
    } else if rest.starts_with(char::is_whitespace) && !after.is_empty() {
        after
    } else {
        return None;
    };
    let expr = expr.trim().trim_end_matches(['}', ';']).trim();
    (!expr.is_empty()).then_some(expr)
}

/// Every `targetSdk` assignment of a build script whose comments were removed.
fn assignments(stripped: &str) -> Vec<Assignment> {
    let mut found = Vec::new();
    // Depth of an open `ext { ... }` block: its entries are definitions.
    let mut ext_depth: i32 = 0;
    for (index, line) in stripped.lines().enumerate() {
        let trimmed = line.trim_start();
        let opens_ext = ext_depth == 0
            && ["ext {", "ext{", "buildscript.ext {", "project.ext {"]
                .iter()
                .any(|p| trimmed.starts_with(p));
        if opens_ext || ext_depth > 0 {
            let opens = i32::try_from(line.matches('{').count()).unwrap_or(i32::MAX);
            let closes = i32::try_from(line.matches('}').count()).unwrap_or(i32::MAX);
            ext_depth = (ext_depth + opens - closes).max(0);
            continue;
        }
        for segment in line.split(['{', ';']) {
            if let Some(expr) = assignment_expr(segment) {
                found.push(Assignment {
                    index,
                    expr: expr.to_string(),
                });
            }
        }
    }
    found
}

fn kind(stripped: &str) -> Kind {
    let mut kind = Kind::Unknown;
    for line in stripped.lines() {
        // `alias(libs.plugins.android.application) apply false` in the root
        // script only puts the plugin on the classpath.
        if line.contains("apply false") {
            continue;
        }
        let l = line.to_ascii_lowercase().replace(['-', '_'], ".");
        let l = l.replace("androidapplication", "android.application");
        let l = l.replace("androidlibrary", "android.library");
        if l.contains("com.android.test") {
            return Kind::Test;
        }
        if l.contains("android.application") {
            kind = Kind::Application;
        } else if kind == Kind::Unknown
            && (l.contains("android.library") || l.contains("dynamic.feature"))
        {
            kind = Kind::Library;
        }
    }
    kind
}

fn has_android_block(stripped: &str) -> bool {
    stripped.lines().any(|l| {
        let t = l.trim_start();
        t.starts_with("android {") || t.starts_with("android{")
    })
}

fn too_low(value: u32, min_sdk: u32) -> (String, String) {
    (
        format!("targetSdk resolves to {value}, below the {min_sdk} that Google Play requires."),
        format!(
            "Raise targetSdk to {min_sdk} or higher and retest the behaviour changes of that API level."
        ),
    )
}

const UNRESOLVED_FIX: &str = "Check the effective targetSdk of the release build (for example with `./gradlew :app:processReleaseManifest`).";

pub(crate) fn scan(root: &Path, inv: &Inventory, min_sdk: u32, sink: &mut Sink) {
    let mut apps_without_target: Vec<String> = Vec::new();
    for rel in &inv.gradle_files {
        let file = rel_str(rel);
        let text = match read_text(&root.join(rel)) {
            Ok(t) => strip_comments(&t, true),
            Err(e) => {
                sink.unreadable(&file, &e);
                continue;
            }
        };
        let kind = kind(&text);
        if kind == Kind::Test || kind == Kind::Library {
            continue;
        }
        let found = assignments(&text);
        if found.is_empty() {
            if kind == Kind::Application && has_android_block(&text) {
                apps_without_target.push(file);
            }
            continue;
        }
        let dir = rel.parent().unwrap_or(Path::new(""));
        for a in found {
            let scope = Scope {
                root,
                dir,
                text: &text,
                skip_line: Some(a.index),
            };
            let line = line_number(a.index);
            match resolve(&a.expr, &scope, 0) {
                Ok(v) if v < min_sdk => {
                    let (message, fix) = too_low(v, min_sdk);
                    sink.critical_if(
                        kind == Kind::Application,
                        "the build script does not show whether this is an application module",
                        &AND_003,
                        &file,
                        line,
                        message,
                        &fix,
                    );
                }
                Ok(_) => {}
                Err(why) => sink.unresolved(
                    &AND_003,
                    &file,
                    line,
                    format!("targetSdk could not be resolved: {why}."),
                    UNRESOLVED_FIX,
                ),
            }
        }
    }
    if !apps_without_target.is_empty() {
        convention(root, inv, min_sdk, &apps_without_target, sink);
    }
    unity(root, inv, min_sdk, sink);
}

/// Application modules that leave `targetSdk` to a convention plugin are fine
/// only when every assignment in `buildSrc`/`build-logic` resolves to an
/// accepted level. A low value there is not a Critical because the scanner
/// cannot tell which modules apply that plugin.
fn convention(root: &Path, inv: &Inventory, min_sdk: u32, apps: &[String], sink: &mut Sink) {
    let mut any = false;
    let mut all_ok = true;
    for rel in &inv.convention_sources {
        let file = rel_str(rel);
        let Ok(raw) = read_text(&root.join(rel)) else {
            // Unreadable convention source: the apps below stay unresolved.
            all_ok = false;
            continue;
        };
        let text = strip_comments(&raw, true);
        // Catalogs and properties are looked up from the directory that holds
        // the included build, which is where the main build keeps them.
        let build_root: PathBuf = rel
            .components()
            .take_while(|c| {
                let c = c.as_os_str();
                c != "buildSrc" && c != "build-logic"
            })
            .collect();
        for a in assignments(&text) {
            any = true;
            let scope = Scope {
                root,
                dir: &build_root,
                text: &text,
                skip_line: Some(a.index),
            };
            let line = line_number(a.index);
            match resolve(&a.expr, &scope, 0) {
                Ok(v) if v >= min_sdk => {}
                Ok(v) => {
                    all_ok = false;
                    sink.unresolved(
                        &AND_003,
                        &file,
                        line,
                        format!("A convention plugin sets targetSdk to {v}, below {min_sdk}; which modules apply it is not known statically."),
                        UNRESOLVED_FIX,
                    );
                }
                Err(why) => {
                    all_ok = false;
                    sink.unresolved(
                        &AND_003,
                        &file,
                        line,
                        format!("targetSdk of a convention plugin could not be resolved: {why}."),
                        UNRESOLVED_FIX,
                    );
                }
            }
        }
    }
    if any && all_ok {
        return;
    }
    if !any {
        for file in apps {
            sink.unresolved(
                &AND_003,
                file,
                None,
                "This application module does not set targetSdk and no convention plugin in buildSrc or build-logic sets it.".to_string(),
                "Set targetSdk explicitly in defaultConfig; without it the Android Gradle plugin falls back to minSdk.",
            );
        }
    }
}

/// Unity keeps the Android target level in `ProjectSettings.asset`; `0` means
/// "highest installed", which depends on the build machine.
fn unity(root: &Path, inv: &Inventory, min_sdk: u32, sink: &mut Sink) {
    for rel in &inv.unity_settings {
        let file = rel_str(rel);
        let text = match read_text(&root.join(rel)) {
            Ok(t) => t,
            Err(e) => {
                sink.unreadable(&file, &e);
                continue;
            }
        };
        let entry = text.lines().enumerate().find_map(|(i, l)| {
            l.trim()
                .strip_prefix("AndroidTargetSdkVersion:")
                .map(|v| (i, v.trim().to_string()))
        });
        let Some((index, value)) = entry else {
            continue;
        };
        let line = line_number(index);
        match value.parse::<u32>() {
            Ok(0) => sink.unresolved(
                &AND_003,
                &file,
                line,
                "Unity AndroidTargetSdkVersion is 0 (automatic: highest SDK installed on the build machine).".to_string(),
                "Pin Target API Level in Player Settings, or verify the SDK installed on the build machine.",
            ),
            Ok(v) if v < min_sdk => {
                let (message, fix) = too_low(v, min_sdk);
                sink.finding(
                    &AND_003,
                    aaf_domain::gate::Severity::Critical,
                    &file,
                    line,
                    message,
                    &fix,
                );
            }
            Ok(_) => {}
            Err(_) => sink.unresolved(
                &AND_003,
                &file,
                line,
                format!("Unity AndroidTargetSdkVersion is `{value}`, not an integer."),
                "Set Target API Level in Player Settings.",
            ),
        }
    }
}
