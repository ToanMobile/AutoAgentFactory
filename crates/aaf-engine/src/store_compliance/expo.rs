//! Expo managed projects: `app.json` carries the Android target SDK
//! (AND-003) and the iOS purpose strings (IOS-001) that the native projects
//! are generated from.

use std::path::Path;

use serde_json::Value;

use super::detect::Inventory;
use super::ios::{is_usage_key, report_purpose};
use super::report::{AND_003, IOS_001, Sink};
use super::text::{line_of_needle, read_text, rel_str};

const DYNAMIC: &str =
    "an app.config file in the same directory can override app.json at build time";

/// `targetSdkVersion` values set directly or through the
/// `expo-build-properties` config plugin.
fn target_sdks(expo: &Value) -> Vec<&Value> {
    let mut found = Vec::new();
    if let Some(v) = expo.pointer("/android/targetSdkVersion") {
        found.push(v);
    }
    let plugins = expo.get("plugins").and_then(Value::as_array);
    for plugin in plugins.into_iter().flatten() {
        let Some([name, options]) = plugin.as_array().map(Vec::as_slice) else {
            continue;
        };
        if name.as_str() == Some("expo-build-properties")
            && let Some(v) = options.pointer("/android/targetSdkVersion")
        {
            found.push(v);
        }
    }
    found
}

fn builds_android(expo: &Value) -> bool {
    match expo.get("platforms").and_then(Value::as_array) {
        Some(list) => list.iter().any(|p| p.as_str() == Some("android")),
        None => true,
    }
}

pub(crate) fn scan(root: &Path, inv: &Inventory, min_sdk: u32, sink: &mut Sink) {
    let mut expo_dirs = Vec::new();
    for rel in &inv.app_jsons {
        let file = rel_str(rel);
        let text = match read_text(&root.join(rel)) {
            Ok(t) => t,
            Err(e) => {
                sink.unreadable(&file, &e);
                continue;
            }
        };
        let json: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                // React Native also has an `app.json`; only report the ones
                // that look like Expo configuration.
                if text.contains("\"expo\"") {
                    sink.unreadable(&file, &format!("invalid JSON ({e})"));
                }
                continue;
            }
        };
        let Some(expo) = json.get("expo").filter(|v| v.is_object()) else {
            continue;
        };
        let dir = rel.parent().unwrap_or(Path::new(""));
        expo_dirs.push(dir.to_path_buf());
        let dynamic = inv.app_configs.iter().any(|c| c.parent() == Some(dir));
        let certain = !dynamic;

        if builds_android(expo) {
            let line = line_of_needle(&text, "\"targetSdkVersion\"");
            let values = target_sdks(expo);
            for v in &values {
                match v.as_u64().and_then(|n| u32::try_from(n).ok()) {
                    Some(n) if n < min_sdk => sink.critical_if(
                        certain,
                        DYNAMIC,
                        &AND_003,
                        &file,
                        line,
                        format!("targetSdkVersion is {n}, below the {min_sdk} that Google Play requires."),
                        &format!("Set android.targetSdkVersion of expo-build-properties to {min_sdk} or higher."),
                    ),
                    Some(_) => {}
                    None => sink.unresolved(
                        &AND_003,
                        &file,
                        line,
                        format!("targetSdkVersion is `{v}`, not an integer."),
                        "Use an integer API level.",
                    ),
                }
            }
            // With a committed `android/` project the Gradle scan decides.
            if values.is_empty() && !root.join(dir).join("android").is_dir() {
                sink.unresolved(
                    &AND_003,
                    &file,
                    None,
                    "app.json does not set targetSdkVersion, so it is the default of the installed Expo SDK.".to_string(),
                    "Pin android.targetSdkVersion with the expo-build-properties plugin, or check the Expo SDK default.",
                );
            }
        }

        let info_plist = expo.pointer("/ios/infoPlist").and_then(Value::as_object);
        for (key, value) in info_plist.into_iter().flatten() {
            if !is_usage_key(key) {
                continue;
            }
            let line = line_of_needle(&text, &format!("\"{key}\""));
            match value.as_str() {
                Some(s) => report_purpose(sink, &file, line, key, s, certain, DYNAMIC),
                None => sink.unresolved(
                    &IOS_001,
                    &file,
                    line,
                    format!("{key} is not a string value."),
                    "Make the purpose string a JSON string.",
                ),
            }
        }
    }

    for rel in &inv.app_configs {
        let dir = rel.parent().unwrap_or(Path::new(""));
        // `app.config.ts` is also an Angular file name; only an Expo project
        // (app.json with `expo`, or the expo package) makes it a build input.
        // No readable package.json next to it: no evidence of Expo.
        let is_expo = expo_dirs.iter().any(|d| d == dir)
            || read_text(&root.join(dir).join("package.json"))
                .map(|t| t.contains("\"expo\""))
                .unwrap_or(false);
        if is_expo {
            sink.unresolved(
                &IOS_001,
                &rel_str(rel),
                None,
                "Dynamic Expo config is evaluated at build time; its purpose strings and target SDK cannot be read statically.".to_string(),
                "Run `npx expo config --type public` and check infoPlist and targetSdkVersion in the output.",
            );
        }
    }
}
