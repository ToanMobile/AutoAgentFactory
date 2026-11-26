//! `{affected_packages}` in `commands.lint` / `commands.test_scoped`: the
//! Cargo workspace members a change set touches, plus every member that
//! depends on them (transitively), as `-p <name> ...`.
//!
//! Read from the manifests on disk without running Cargo. Anything this
//! cannot account for widens to `--workspace`, never narrows: a `--full` or
//! `--release` gate, a change to a manifest, `Cargo.lock`, a `build.rs`,
//! `.cargo/`, the toolchain file, any unnormalizable path, any path outside
//! a member directory, any path owned by a root `[package]` outside its own
//! `src/`, `tests/`, `benches/`, or `examples/` trees, any path under a
//! deeper `Cargo.toml` than its owning member (such as an unlisted nested
//! crate or fixture), a `path` dependency targeting a directory outside the
//! listed workspace members (or unnormalizable), a root `[patch]` or
//! `[replace]` entry with `path`, a `[patch]` or `paths` in the repo's
//! `.cargo/config(.toml)` (or such a file that is unreadable or not TOML), a
//! members glob other than `dir/*`, and an unreadable manifest.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path};

use aaf_domain::config::CommandsConfig;
use aaf_domain::gate::GateMode;

use super::changes::Change;

/// The placeholder replaced in `lint` and `test_scoped`.
pub const AFFECTED_PACKAGES: &str = "{affected_packages}";
/// What the placeholder becomes when the change set cannot be narrowed.
pub const WHOLE_WORKSPACE: &str = "--workspace";

/// `commands` with [`AFFECTED_PACKAGES`] substituted, and a note saying what
/// it became; `None` when no command uses the placeholder.
pub fn expand(
    commands: &CommandsConfig,
    mode: GateMode,
    repo_root: &Path,
    changes: &[Change],
) -> Option<(CommandsConfig, String)> {
    let uses = |cmd: &str| cmd.contains(AFFECTED_PACKAGES);
    if ![
        &commands.lint,
        &commands.test_scoped,
        &commands.lint_full,
        &commands.test_full,
    ]
    .iter()
    .any(|cmd| uses(cmd))
    {
        return None;
    }
    let selected = if mode == GateMode::Scoped {
        affected_packages(repo_root, changes)
    } else {
        None
    };
    let (args, note) = match selected {
        Some(names) if !names.is_empty() => {
            let args = names
                .iter()
                .map(|n| format!("-p {n}"))
                .collect::<Vec<_>>()
                .join(" ");
            let note = format!(
                "affected packages: {} of the workspace ({})",
                names.len(),
                names.iter().cloned().collect::<Vec<_>>().join(", ")
            );
            (args, note)
        }
        _ => (
            WHOLE_WORKSPACE.to_string(),
            "affected packages: whole workspace".to_string(),
        ),
    };
    let mut out = commands.clone();
    out.lint = out.lint.replace(AFFECTED_PACKAGES, &args);
    out.test_scoped = out.test_scoped.replace(AFFECTED_PACKAGES, &args);
    // The full commands never narrow.
    out.lint_full = out.lint_full.replace(AFFECTED_PACKAGES, WHOLE_WORKSPACE);
    out.test_full = out.test_full.replace(AFFECTED_PACKAGES, WHOLE_WORKSPACE);
    Some((out, note))
}

/// `cfg` with [`expand`] applied to its commands, for code that runs a
/// command itself from the configuration (the red->green oracle).
pub fn expanded_cfg<'a>(
    cfg: &'a aaf_domain::config::RepoConfig,
    mode: GateMode,
    repo_root: &Path,
    changes: &[Change],
) -> std::borrow::Cow<'a, aaf_domain::config::RepoConfig> {
    match expand(&cfg.commands, mode, repo_root, changes) {
        Some((commands, _)) => {
            let mut owned = cfg.clone();
            owned.commands = commands;
            std::borrow::Cow::Owned(owned)
        }
        None => std::borrow::Cow::Borrowed(cfg),
    }
}

/// [`expand`], or `commands` as they are; the note goes into `notes`.
pub fn expanded_or<'a>(
    commands: &'a CommandsConfig,
    mode: GateMode,
    repo_root: &Path,
    changes: &[Change],
    notes: &mut Vec<String>,
) -> std::borrow::Cow<'a, CommandsConfig> {
    match expand(commands, mode, repo_root, changes) {
        Some((cmds, note)) => {
            notes.push(note);
            std::borrow::Cow::Owned(cmds)
        }
        None => std::borrow::Cow::Borrowed(commands),
    }
}

#[derive(Debug)]
struct Member {
    name: String,
    /// Relative to the workspace root, `/`-separated, no trailing slash.
    dir: String,
    /// Directories (same form) of the members this one depends on by path.
    deps: BTreeSet<String>,
}

/// Names of the touched members and their reverse dependencies, sorted;
/// `None` = cannot narrow (run the whole workspace).
pub fn affected_packages(repo_root: &Path, changes: &[Change]) -> Option<BTreeSet<String>> {
    let members = read_workspace(repo_root)?;
    let mut touched: BTreeSet<&str> = BTreeSet::new();
    for raw_path in changes
        .iter()
        .flat_map(|c| std::iter::once(c.path.as_str()).chain(c.old_path.as_deref()))
    {
        let path = normalize(raw_path)?;
        if path.is_empty() || widens(&path) {
            return None;
        }
        let owner = members
            .iter()
            .filter(|m| m.dir.is_empty() || path.starts_with(&format!("{}/", m.dir)))
            .max_by_key(|m| m.dir.len())?;
        if owner.dir.is_empty() && !is_root_package_source(&path) {
            return None;
        }
        if has_deeper_manifest(repo_root, &owner.dir, &path) {
            return None;
        }
        touched.insert(owner.dir.as_str());
    }
    // Reverse dependencies, to a fixed point.
    let mut dirs: BTreeSet<&str> = touched;
    loop {
        let before = dirs.len();
        for m in &members {
            if m.deps.iter().any(|d| dirs.contains(d.as_str())) {
                dirs.insert(m.dir.as_str());
            }
        }
        if dirs.len() == before {
            break;
        }
    }
    Some(
        members
            .iter()
            .filter(|m| dirs.contains(m.dir.as_str()))
            .map(|m| m.name.clone())
            .collect(),
    )
}

/// Paths whose change can affect any member, or that this module does not model.
fn widens(path: &str) -> bool {
    let file = path.rsplit('/').next().unwrap_or(path);
    matches!(
        file,
        "Cargo.toml" | "Cargo.lock" | "build.rs" | "rust-toolchain" | "rust-toolchain.toml"
    ) || path.starts_with(".cargo/")
        || path.contains("/.cargo/")
}

/// For a root `[package]` (`dir == ""`), only paths inside its standard source
/// trees narrow to the root package; everything else at the repo root (docs,
/// shared fixtures, submodules/gitlinks, etc.) widens to the whole workspace.
/// Note: `build.rs` is already widened unconditionally by [`widens`].
fn is_root_package_source(path: &str) -> bool {
    path.starts_with("src/")
        || path.starts_with("tests/")
        || path.starts_with("benches/")
        || path.starts_with("examples/")
}

/// `true` if `path` itself (when it is a directory or gitlink) or any ancestor
/// strictly deeper than `owner_dir` contains a regular `Cargo.toml` file.
fn has_deeper_manifest(repo_root: &Path, owner_dir: &str, path: &str) -> bool {
    let mut cur = path;
    while cur.len() > owner_dir.len() {
        if repo_root.join(cur).join("Cargo.toml").is_file() {
            return true;
        }
        let Some((parent, _)) = cur.rsplit_once('/') else {
            break;
        };
        cur = parent;
    }
    false
}

/// `true` if the root manifest declares a `[patch]` (for any registry) or
/// `[replace]` entry with a `path`, or has an unexpected non-table section.
fn has_path_patch_or_replace(root: &toml::Table) -> bool {
    ["patch", "replace"].iter().any(|section| {
        root.get(*section)
            .is_some_and(|v| v.as_table().is_none_or(has_path_key))
    })
}

fn has_path_key(table: &toml::Table) -> bool {
    table.iter().any(|(k, v)| match v.as_table() {
        Some(sub) => has_path_key(sub),
        None => k == "path",
    })
}

fn read_toml(path: &Path) -> Option<toml::Table> {
    fs::read_to_string(path).ok()?.parse::<toml::Table>().ok()
}

/// `true` if the repo's `.cargo/config.toml` and legacy `.cargo/config` are
/// absent, or parse as TOML without a top-level `patch` or `paths` (either
/// can override a dependency with a local crate). Any read error other than
/// "not found", or a parse error, counts as `false`.
fn cargo_config_allows_narrowing(repo_root: &Path) -> bool {
    ["config.toml", "config"].iter().all(|name| {
        match fs::read_to_string(repo_root.join(".cargo").join(name)) {
            Err(e) => e.kind() == std::io::ErrorKind::NotFound,
            Ok(text) => text
                .parse::<toml::Table>()
                .is_ok_and(|cfg| !cfg.contains_key("patch") && !cfg.contains_key("paths")),
        }
    })
}

fn read_workspace(repo_root: &Path) -> Option<Vec<Member>> {
    let root = read_toml(&repo_root.join("Cargo.toml"))?;
    if has_path_patch_or_replace(&root) || !cargo_config_allows_narrowing(repo_root) {
        return None;
    }
    let ws = root.get("workspace")?.as_table()?;
    let mut dirs: Vec<String> = Vec::new();
    for entry in ws.get("members")?.as_array()? {
        let entry = normalize(entry.as_str()?)?;
        if let Some(parent) = entry.strip_suffix("/*") {
            let mut found: Vec<String> = fs::read_dir(repo_root.join(parent))
                .ok()?
                .filter_map(|e| e.ok())
                .filter(|e| e.path().join("Cargo.toml").is_file())
                .filter_map(|e| e.file_name().into_string().ok())
                .map(|name| format!("{parent}/{name}"))
                .collect();
            found.sort();
            dirs.extend(found);
        } else if entry.contains(['*', '?', '[']) {
            return None;
        } else {
            dirs.push(entry);
        }
    }
    // Cargo leaves excluded directories out of the workspace, so `-p` on
    // one would fail: they are no member here either.
    let excluded: Vec<String> = ws
        .get("exclude")
        .and_then(toml::Value::as_array)
        .map(|list| list.iter().filter_map(|e| normalize(e.as_str()?)).collect())
        .unwrap_or_default();
    dirs.retain(|dir| {
        !excluded
            .iter()
            .any(|ex| dir == ex || dir.starts_with(&format!("{ex}/")))
    });
    if root.contains_key("package") {
        dirs.push(String::new());
    }
    let known_dirs: BTreeSet<String> = dirs.into_iter().collect();
    let mut ws_deps: BTreeMap<String, String> = BTreeMap::new();
    if let Some(deps_val) = ws.get("dependencies") {
        for (key, spec) in deps_val.as_table()? {
            if let Some(path_val) = spec.as_table().and_then(|t| t.get("path")) {
                let target = normalize(path_val.as_str()?)?;
                if !known_dirs.contains(&target) {
                    return None;
                }
                ws_deps.insert(key.clone(), target);
            }
        }
    }
    let mut members = Vec::new();
    for dir in &known_dirs {
        let manifest = if dir.is_empty() {
            root.clone()
        } else {
            read_toml(&repo_root.join(dir).join("Cargo.toml"))?
        };
        let name = manifest.get("package")?.as_table()?.get("name")?.as_str()?;
        let mut deps = BTreeSet::new();
        for table in dependency_tables(&manifest)? {
            for (key, spec) in table {
                let Some(spec) = spec.as_table() else {
                    continue;
                };
                if let Some(path_val) = spec.get("path") {
                    let target = resolve_rel(dir, path_val.as_str()?)?;
                    if !known_dirs.contains(&target) {
                        return None;
                    }
                    deps.insert(target);
                } else if spec.get("workspace").and_then(toml::Value::as_bool) == Some(true)
                    && let Some(p) = ws_deps.get(key)
                {
                    deps.insert(p.clone());
                }
            }
        }
        members.push(Member {
            name: name.to_string(),
            dir: dir.clone(),
            deps,
        });
    }
    Some(members)
}

/// `[dependencies]`, `[dev-dependencies]`, `[build-dependencies]`, the
/// underscore spellings Cargo still accepts (`[dev_dependencies]`,
/// `[build_dependencies]`), and their `[target.'cfg(..)'.*]` forms.
fn dependency_tables(manifest: &toml::Table) -> Option<Vec<&toml::Table>> {
    const KINDS: [&str; 5] = [
        "dependencies",
        "dev-dependencies",
        "build-dependencies",
        "dev_dependencies",
        "build_dependencies",
    ];
    let mut out: Vec<&toml::Table> = Vec::new();
    for k in KINDS {
        if let Some(v) = manifest.get(k) {
            out.push(v.as_table()?);
        }
    }
    if let Some(targets_val) = manifest.get("target") {
        for t_val in targets_val.as_table()?.values() {
            let t = t_val.as_table()?;
            for k in KINDS {
                if let Some(v) = t.get(k) {
                    out.push(v.as_table()?);
                }
            }
        }
    }
    Some(out)
}

fn resolve_rel(dir: &str, rel: &str) -> Option<String> {
    if Path::new(rel).is_absolute() {
        return None;
    }
    normalize_path(&Path::new(dir).join(rel))
}

/// `a/./b/../c/` -> `a/c`; `None` for an absolute path or one leaving the root.
fn normalize(path: &str) -> Option<String> {
    normalize_path(Path::new(path))
}

fn normalize_path(path: &Path) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for c in path.components() {
        match c {
            Component::Normal(s) => parts.push(s.to_str()?.to_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(parts.join("/"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
