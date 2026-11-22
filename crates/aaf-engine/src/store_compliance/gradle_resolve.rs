//! Static resolution of a Gradle value expression to an API level: integer
//! literal, version catalog entry, Gradle property or a named definition.
//! Everything else is an error that the caller reports as unresolved.

use std::path::Path;

use super::text::{read_text, rel_str, strip_comments};

const MAX_RESOLVE_DEPTH: u8 = 4;

pub(super) fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn is_ident(s: &str) -> bool {
    !s.is_empty() && s.chars().all(is_ident_char) && !s.starts_with(|c: char| c.is_ascii_digit())
}

/// Where an expression is being resolved.
pub(super) struct Scope<'a> {
    pub root: &'a Path,
    /// Directory of the build script, relative to `root`.
    pub dir: &'a Path,
    /// Comment-stripped text of the build script.
    pub text: &'a str,
    /// Line of the assignment itself, so it is not taken as its own definition.
    pub skip_line: Option<usize>,
}

/// `dir`, then each ancestor up to the scanned root.
fn ancestors(dir: &Path) -> impl Iterator<Item = &Path> {
    dir.ancestors()
}

fn strip_wrappers(expr: &str) -> &str {
    let mut e = expr.trim();
    loop {
        let before = e;
        e = e.trim().trim_end_matches([';', ',']).trim();
        for suffix in [
            "!!",
            ".toInt()",
            ".toInteger()",
            ".toString()",
            ".get()",
            ".getAsInt()",
            ".requiredVersion",
            ".orNull",
            " as int",
            " as Int",
            " as Integer",
            " as String?",
            " as String",
        ] {
            if let Some(s) = e.strip_suffix(suffix) {
                e = s.trim();
            }
        }
        for prefix in ["Integer.parseInt(", "Integer.valueOf(", "("] {
            if let Some(inner) = e.strip_prefix(prefix).and_then(|s| s.strip_suffix(')')) {
                // Only when the parentheses we removed were a matching pair.
                let mut depth = 0i32;
                let balanced = inner.chars().all(|c| {
                    match c {
                        '(' => depth += 1,
                        ')' => depth -= 1,
                        _ => {}
                    }
                    depth >= 0
                });
                if balanced && depth == 0 {
                    e = inner.trim();
                }
            }
        }
        if e == before {
            return e;
        }
    }
}

/// `"name"` or `'name'` → `name`.
fn unquote(s: &str) -> Option<&str> {
    let s = s.trim();
    let inner = s
        .strip_prefix('"')
        .and_then(|r| r.strip_suffix('"'))
        .or_else(|| s.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')))?;
    (!inner.contains(['"', '\''])).then_some(inner)
}

/// `call("name")` → `name` for one of the given call heads.
fn call_arg<'a>(expr: &'a str, heads: &[&str]) -> Option<&'a str> {
    heads.iter().find_map(|head| {
        let inner = expr
            .strip_prefix(head)?
            .strip_prefix('(')?
            .strip_suffix(')')?;
        unquote(inner)
    })
}

pub(super) fn resolve(expr: &str, scope: &Scope<'_>, depth: u8) -> Result<u32, String> {
    if depth > MAX_RESOLVE_DEPTH {
        return Err(format!("`{expr}` is defined through too many indirections"));
    }
    let e = strip_wrappers(expr);
    let digits = unquote(e).unwrap_or(e);
    if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
        return digits
            .parse::<u32>()
            .map_err(|_| format!("`{digits}` is not a valid API level"));
    }
    if e.starts_with("flutter.") {
        return Err(format!(
            "`{e}` is chosen by the installed Flutter SDK, not by this repository"
        ));
    }
    if let Some(key) = e.strip_prefix("libs.versions.") {
        return catalog(key, scope);
    }
    if let Some(key) = call_arg(e, &["libs.findVersion"]) {
        return catalog(key, scope);
    }
    let unowned = e
        .strip_prefix("rootProject.")
        .or_else(|| e.strip_prefix("project."))
        .unwrap_or(e);
    let property_heads = [
        "findProperty",
        "property",
        "providers.gradleProperty",
        "gradleProperty",
    ];
    let indexed = |owner: &str| {
        unowned
            .strip_prefix(owner)?
            .strip_prefix('[')?
            .strip_suffix(']')
            .and_then(unquote)
    };
    if let Some(name) = call_arg(unowned, &property_heads).or_else(|| indexed("properties")) {
        return gradle_property(name, scope);
    }
    let name = indexed("extra")
        .or_else(|| indexed("ext"))
        .or_else(|| call_arg(unowned, &["ext.get", "extra.get"]))
        .or_else(|| unowned.strip_prefix("ext."))
        .or_else(|| unowned.strip_prefix("extra."))
        .unwrap_or(unowned);
    if is_ident(name) {
        return definition(name, scope, depth);
    }
    Err(format!("`{e}` cannot be resolved without running Gradle"))
}

/// Look `key` up in the nearest `gradle/libs.versions.toml`. Gradle exposes a
/// catalog alias `a-b_c` as the accessor `a.b.c`.
fn catalog(key: &str, scope: &Scope<'_>) -> Result<u32, String> {
    let normalise = |s: &str| s.replace(['-', '_'], ".");
    let wanted = normalise(key);
    for dir in ancestors(scope.dir) {
        let rel = dir.join("gradle").join("libs.versions.toml");
        let path = scope.root.join(&rel);
        if !path.is_file() {
            continue;
        }
        let file = rel_str(&rel);
        let table = read_text(&path)
            .and_then(|t| {
                t.parse::<toml::Table>()
                    .map_err(|e| e.message().to_string())
            })
            .map_err(|e| format!("{file} could not be parsed ({e})"))?;
        let value = table
            .get("versions")
            .and_then(toml::Value::as_table)
            .and_then(|v| v.iter().find(|(k, _)| normalise(k) == wanted))
            .map(|(_, v)| v)
            .ok_or_else(|| format!("version `{key}` is not defined in {file}"))?;
        return match value {
            toml::Value::Integer(n) => {
                u32::try_from(*n).map_err(|_| format!("version `{key}` in {file} is out of range"))
            }
            toml::Value::String(s) => s
                .trim()
                .parse::<u32>()
                .map_err(|_| format!("version `{key}` in {file} is `{s}`, not an integer")),
            _ => Err(format!("version `{key}` in {file} is not a plain value")),
        };
    }
    Err(format!(
        "`libs.versions.{key}` is used but no gradle/libs.versions.toml was found"
    ))
}

/// Subproject `gradle.properties` wins over the root one.
fn gradle_property(name: &str, scope: &Scope<'_>) -> Result<u32, String> {
    for dir in ancestors(scope.dir) {
        let rel = dir.join("gradle.properties");
        let path = scope.root.join(&rel);
        if !path.is_file() {
            continue;
        }
        let file = rel_str(&rel);
        let text = read_text(&path).map_err(|e| format!("{file} could not be read ({e})"))?;
        // The last assignment of a key wins in a properties file.
        let value = text.lines().rev().find_map(|line| {
            let line = line.trim_start();
            if line.starts_with(['#', '!']) {
                return None;
            }
            let (k, v) = line.split_once(['=', ':'])?;
            (k.trim() == name).then(|| v.trim().to_string())
        });
        if let Some(v) = value {
            return v
                .parse::<u32>()
                .map_err(|_| format!("property `{name}` in {file} is `{v}`, not an integer"));
        }
    }
    Err(format!(
        "property `{name}` is not defined in any gradle.properties of the repository"
    ))
}

/// The right-hand side when `line` defines `name` (`val name = v`,
/// `ext.name = v`, `name = v` inside `ext {}`, `extra["name"] = v`,
/// `set("name", v)`, `val name by extra(v)`).
fn definition_rhs<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let mut s = line.trim();
    loop {
        let before = s;
        for word in [
            "private ",
            "internal ",
            "const ",
            "final ",
            "static ",
            "val ",
            "var ",
            "def ",
            "int ",
            "Integer ",
            "String ",
        ] {
            s = s.strip_prefix(word).unwrap_or(s).trim_start();
        }
        for owner in ["rootProject.", "project.", "ext.", "extra."] {
            s = s.strip_prefix(owner).unwrap_or(s);
        }
        if s == before {
            break;
        }
    }
    let assigned = |rest: &'a str| -> Option<&'a str> {
        let rest = rest.trim_start();
        // Skip a Kotlin type annotation: `val x: Int = 34`.
        let rest = match rest.strip_prefix(':') {
            Some(typed) => &typed[typed.find('=')?..],
            None => rest,
        };
        let rhs = rest.strip_prefix('=')?;
        (!rhs.starts_with('=')).then_some(rhs.trim())
    };
    if let Some(rest) = s.strip_prefix(name) {
        if rest.starts_with(is_ident_char) {
            return None;
        }
        if let Some(by) = rest.trim_start().strip_prefix("by extra(") {
            return by.rsplit_once(')').map(|(inner, _)| inner.trim());
        }
        return assigned(rest);
    }
    for (open, close) in [("extra[", ']'), ("ext[", ']'), ("[", ']')] {
        if let Some((key, rest)) = s.strip_prefix(open).and_then(|r| r.split_once(close)) {
            return (unquote(key) == Some(name))
                .then(|| assigned(rest))
                .flatten();
        }
    }
    let (key, value) = s
        .strip_prefix("set(")?
        .rsplit_once(')')?
        .0
        .split_once(',')?;
    (unquote(key) == Some(name)).then_some(value.trim())
}

/// Resolve an identifier through its definition in the build script itself,
/// then in the build scripts of the enclosing directories, then as a Gradle
/// property (Groovy exposes those as bare identifiers).
fn definition(name: &str, scope: &Scope<'_>, depth: u8) -> Result<u32, String> {
    let resolve_in = |text: &str, dir: &Path, skip: Option<usize>| -> Option<Result<u32, String>> {
        let inner = Scope {
            root: scope.root,
            dir,
            text,
            skip_line: None,
        };
        let mut values: Vec<Result<u32, String>> = text
            .lines()
            .enumerate()
            .filter(|(i, _)| Some(*i) != skip)
            .filter_map(|(_, line)| definition_rhs(line, name))
            // `targetSdkVersion = targetSdkVersion` defines nothing.
            .filter(|rhs| strip_wrappers(rhs) != name)
            .map(|rhs| resolve(rhs, &inner, depth + 1))
            .collect();
        let first = values.pop()?;
        if values.iter().any(|v| *v != first) {
            return Some(Err(format!("`{name}` has more than one definition")));
        }
        Some(first)
    };
    if let Some(found) = resolve_in(scope.text, scope.dir, scope.skip_line) {
        return found;
    }
    for dir in ancestors(scope.dir).skip(1) {
        for file in ["build.gradle.kts", "build.gradle"] {
            // A missing or unreadable parent script simply defines nothing here;
            // the lookup continues and ends in an explicit "not defined".
            let Ok(text) = read_text(&scope.root.join(dir).join(file)) else {
                continue;
            };
            if let Some(found) = resolve_in(&strip_comments(&text, true), dir, None) {
                return found;
            }
        }
    }
    gradle_property(name, scope).map_err(|_| {
        format!("`{name}` is not defined as a literal in any build script or gradle.properties")
    })
}
