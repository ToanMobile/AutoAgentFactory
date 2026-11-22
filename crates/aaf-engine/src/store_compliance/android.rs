//! `AndroidManifest.xml` rules: AND-001, AND-002, AND-101, AND-102, AND-103.
//! Target SDK resolution (AND-003) lives in `gradle.rs`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use aaf_domain::gate::Severity;
use quick_xml::XmlVersion;
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;

use super::detect::Inventory;
use super::report::{AND_001, AND_002, AND_101, AND_102, AND_103, Sink};
use super::text::{line_of, read_text, rel_str};

const ANDROID_NS: &str = "http://schemas.android.com/apk/res/android";
const TOOLS_NS: &str = "http://schemas.android.com/tools";

/// Google Play Permissions policy: each of these needs a Play Console
/// declaration or is limited to a narrow set of approved use cases.
const SENSITIVE_PERMISSIONS: [&str; 6] = [
    "READ_SMS",
    "READ_CALL_LOG",
    "MANAGE_EXTERNAL_STORAGE",
    "REQUEST_INSTALL_PACKAGES",
    "QUERY_ALL_PACKAGES",
    "ACCESS_BACKGROUND_LOCATION",
];

/// Whether a manifest is certain to be merged into the build that ships.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Placement {
    /// `src/main` or `src/release`: always part of the release build.
    Shipping,
    /// Another source set (a flavor): ships only in some variants.
    Variant,
    /// Not under `src/<set>/` (Unity plugin folder, tool, legacy layout).
    Loose,
    /// Debug, test and benchmark source sets never reach a store.
    Skip,
}

pub(crate) fn classify(rel: &Path) -> (PathBuf, Placement) {
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let n = parts.len();
    if n >= 3 && parts[n - 3] == "src" {
        let set = parts[n - 2].to_ascii_lowercase();
        let placement = if set == "main" || set == "release" {
            Placement::Shipping
        } else if set.contains("debug")
            || set.starts_with("test")
            || set.starts_with("androidtest")
            || set.contains("benchmark")
        {
            Placement::Skip
        } else {
            Placement::Variant
        };
        return (parts[..n - 3].iter().collect(), placement);
    }
    (parts[..n - 1].iter().collect(), Placement::Loose)
}

#[derive(Default, Debug)]
struct Application {
    line: u32,
    debuggable: Option<String>,
    cleartext: Option<String>,
    has_network_security_config: bool,
    allow_backup: Option<String>,
}

#[derive(Debug)]
struct Component {
    tag: String,
    name: String,
    line: u32,
    has_intent_filter: bool,
    has_exported: bool,
}

#[derive(Default, Debug)]
struct Manifest {
    application: Option<Application>,
    components: Vec<Component>,
    permissions: Vec<(String, u32)>,
}

/// Attributes of one element, keyed as `android:name` / `tools:node` whatever
/// prefix the document binds to those namespaces.
struct Attrs(BTreeMap<String, String>);

impl Attrs {
    fn read(e: &BytesStart<'_>, android: &str, tools: &str) -> Result<Attrs, String> {
        let mut map = BTreeMap::new();
        for attr in e.attributes() {
            let attr = attr.map_err(|e| e.to_string())?;
            let value = attr
                .normalized_value(XmlVersion::Implicit1_0)
                .map_err(|e| e.to_string())?
                .into_owned();
            let key = attr.key.0;
            let key = match key.split_once(':') {
                Some((p, local)) if p == android => format!("android:{local}"),
                Some((p, local)) if p == tools => format!("tools:{local}"),
                _ => key.to_string(),
            };
            map.insert(key, value);
        }
        Ok(Attrs(map))
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    /// The manifest merger drops this element from the final manifest.
    fn removed(&self) -> bool {
        matches!(self.get("tools:node"), Some("remove" | "removeAll"))
    }
}

/// Prefixes bound to the Android and tools namespaces on the root element.
fn namespace_prefixes(e: &BytesStart<'_>) -> (String, String) {
    let mut android = "android".to_string();
    let mut tools = "tools".to_string();
    for attr in e.attributes().flatten() {
        if let Some(prefix) = attr.key.0.strip_prefix("xmlns:") {
            if attr.value == ANDROID_NS {
                android = prefix.to_string();
            } else if attr.value == TOOLS_NS {
                tools = prefix.to_string();
            }
        }
    }
    (android, tools)
}

fn parse(text: &str) -> Result<Manifest, String> {
    let mut reader = Reader::from_str(text);
    let mut manifest = Manifest::default();
    let mut stack: Vec<String> = Vec::new();
    let mut prefixes = ("android".to_string(), "tools".to_string());
    // Index into `manifest.components` of the component being read.
    let mut current: Option<usize> = None;
    let mut saw_root = false;
    loop {
        let start = usize::try_from(reader.buffer_position()).unwrap_or(usize::MAX);
        let event = reader.read_event().map_err(|e| {
            let line = line_of(text, usize::try_from(reader.error_position()).unwrap_or(0));
            format!("XML error near line {line}: {e}")
        })?;
        let (e, is_empty) = match event {
            Event::Start(e) => (e, false),
            Event::Empty(e) => (e, true),
            Event::End(_) => {
                if stack.len() == 3 {
                    current = None;
                }
                stack.pop();
                continue;
            }
            Event::Eof => break,
            _ => continue,
        };
        let tag = e.name().0.to_string();
        let line = line_of(text, start);
        if stack.is_empty() {
            if tag != "manifest" {
                return Err(format!("root element is <{tag}>, not <manifest>"));
            }
            saw_root = true;
            prefixes = namespace_prefixes(&e);
        }
        let attrs = Attrs::read(&e, &prefixes.0, &prefixes.1)
            .map_err(|err| format!("bad attribute near line {line}: {err}"))?;
        let parent = stack.last().map(String::as_str).unwrap_or("");
        match (stack.len(), parent, tag.as_str()) {
            (1, "manifest", "application") if !attrs.removed() => {
                manifest.application = Some(Application {
                    line,
                    debuggable: attrs.get("android:debuggable").map(str::to_string),
                    cleartext: attrs
                        .get("android:usesCleartextTraffic")
                        .map(str::to_string),
                    has_network_security_config: attrs
                        .get("android:networkSecurityConfig")
                        .is_some(),
                    allow_backup: attrs.get("android:allowBackup").map(str::to_string),
                });
            }
            (1, "manifest", "uses-permission" | "uses-permission-sdk-23") if !attrs.removed() => {
                if let Some(name) = attrs.get("android:name") {
                    manifest.permissions.push((name.to_string(), line));
                }
            }
            (2, "application", "activity" | "activity-alias" | "service" | "receiver")
                if !attrs.removed() =>
            {
                manifest.components.push(Component {
                    tag: tag.clone(),
                    name: attrs.get("android:name").unwrap_or("").to_string(),
                    line,
                    has_intent_filter: false,
                    has_exported: attrs.get("android:exported").is_some(),
                });
                if !is_empty {
                    current = Some(manifest.components.len() - 1);
                }
            }
            (3, _, "intent-filter") if !attrs.removed() => {
                if let Some(c) = current.and_then(|i| manifest.components.get_mut(i)) {
                    c.has_intent_filter = true;
                }
            }
            _ => {}
        }
        if !is_empty {
            stack.push(tag);
        }
    }
    if !saw_root {
        return Err("no <manifest> element".to_string());
    }
    if !stack.is_empty() {
        return Err(format!("unclosed <{}> element", stack.join("><")));
    }
    Ok(manifest)
}

/// `.Main` and `com.example.Main` name the same class once the package is
/// applied, so components are matched on the class name alone.
fn short_name(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

/// A module applying `com.android.test` only produces a test APK.
fn is_test_module(root: &Path, module: &Path) -> bool {
    ["build.gradle.kts", "build.gradle"].iter().any(|f| {
        // No readable build file means no evidence of a test module.
        read_text(&root.join(module).join(f))
            .map(|t| t.contains("com.android.test"))
            .unwrap_or(false)
    })
}

pub(crate) fn scan(root: &Path, inv: &Inventory, sink: &mut Sink) {
    let mut modules: BTreeMap<PathBuf, Vec<(String, Placement, Manifest)>> = BTreeMap::new();
    for rel in &inv.manifests {
        let (module, placement) = classify(rel);
        if placement == Placement::Skip {
            continue;
        }
        let file = rel_str(rel);
        match read_text(&root.join(rel)).and_then(|t| parse(&t)) {
            Ok(m) => modules
                .entry(module)
                .or_default()
                .push((file, placement, m)),
            Err(e) => sink.unreadable(&file, &e),
        }
    }
    for (module, manifests) in &modules {
        if is_test_module(root, module) {
            continue;
        }
        // An overlay manifest of the same module may supply `android:exported`
        // for a component another manifest declares.
        let exported: BTreeSet<&str> = manifests
            .iter()
            .flat_map(|(_, _, m)| &m.components)
            .filter(|c| c.has_exported && !c.name.is_empty())
            .map(|c| short_name(&c.name))
            .collect();
        for (file, placement, manifest) in manifests {
            check(file, *placement, manifest, &exported, sink);
        }
    }
}

fn check(
    file: &str,
    placement: Placement,
    manifest: &Manifest,
    exported: &BTreeSet<&str>,
    sink: &mut Sink,
) {
    let certain = placement == Placement::Shipping;
    let uncertainty = match placement {
        Placement::Variant => "flavor source set: cannot tell which variants ship it",
        _ => "manifest is outside src/main and src/release: the build tool may rewrite or drop it",
    };
    if let Some(app) = &manifest.application {
        match app.debuggable.as_deref() {
            Some("true") => sink.critical_if(
                certain,
                uncertainty,
                &AND_001,
                file,
                Some(app.line),
                "<application> sets android:debuggable=\"true\", which Google Play rejects in a release upload.".to_string(),
                "Remove android:debuggable from this manifest and let the debug build type set it.",
            ),
            Some(v) if v != "false" => sink.unresolved(
                &AND_001,
                file,
                Some(app.line),
                format!("android:debuggable is `{v}`, which is only known at build time."),
                "Make sure the release build resolves this value to false.",
            ),
            _ => {}
        }
        if app.cleartext.as_deref() == Some("true") && !app.has_network_security_config {
            sink.finding(
                &AND_101,
                Severity::Warn,
                file,
                Some(app.line),
                "android:usesCleartextTraffic=\"true\" allows unencrypted HTTP for every host.".to_string(),
                "Add android:networkSecurityConfig and allow cleartext only for the hosts that need it.",
            );
        }
        if app.allow_backup.as_deref() == Some("true") {
            sink.finding(
                &AND_103,
                Severity::Warn,
                file,
                Some(app.line),
                "android:allowBackup=\"true\" copies app data to the user's cloud backup.".to_string(),
                "Set allowBackup to false or add dataExtractionRules that exclude tokens and private files.",
            );
        }
    }
    for c in &manifest.components {
        if !c.has_intent_filter || c.has_exported {
            continue;
        }
        if !c.name.is_empty() && exported.contains(short_name(&c.name)) {
            continue;
        }
        sink.critical_if(
            certain,
            uncertainty,
            &AND_002,
            file,
            Some(c.line),
            format!(
                "<{} android:name=\"{}\"> has an <intent-filter> but no android:exported attribute.",
                c.tag, c.name
            ),
            "Add android:exported=\"true\" if other apps must start it, otherwise android:exported=\"false\".",
        );
    }
    for (name, line) in &manifest.permissions {
        let short = short_name(name);
        if SENSITIVE_PERMISSIONS.contains(&short) {
            sink.finding(
                &AND_102,
                Severity::Warn,
                file,
                Some(*line),
                format!("Permission {short} is restricted by Google Play policy."),
                "Remove the permission, or file the Play Console permission declaration with the approved use case.",
            );
        }
    }
}
