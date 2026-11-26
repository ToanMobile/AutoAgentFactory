//! `{affected_packages}` in `commands.lint` / `commands.test_scoped`: the
//! Cargo workspace members a change set touches, plus every member that
//! depends on them (transitively), as `-p <name> ...`.
//!
//! Read from the manifests on disk without running Cargo. Anything this
//! cannot account for widens to `--workspace`, never narrows: a `--full` or
//! `--release` gate, a change to a manifest, `Cargo.lock`, a `build.rs`,
//! `.cargo/`, the toolchain file or any path outside a member directory, a
//! members glob other than `dir/*`, and an unreadable manifest.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

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
    if !uses(&commands.lint) && !uses(&commands.test_scoped) {
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
    for path in changes
        .iter()
        .flat_map(|c| std::iter::once(c.path.as_str()).chain(c.old_path.as_deref()))
    {
        if widens(path) {
            return None;
        }
        let owner = members
            .iter()
            .filter(|m| m.dir.is_empty() || path.starts_with(&format!("{}/", m.dir)))
            .max_by_key(|m| m.dir.len())?;
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

fn read_toml(path: &Path) -> Option<toml::Table> {
    fs::read_to_string(path).ok()?.parse::<toml::Table>().ok()
}

fn read_workspace(repo_root: &Path) -> Option<Vec<Member>> {
    let root = read_toml(&repo_root.join("Cargo.toml"))?;
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
    if root.contains_key("package") {
        dirs.push(String::new());
    }
    let ws_deps: BTreeMap<String, String> = ws
        .get("dependencies")
        .and_then(toml::Value::as_table)
        .map(|t| {
            t.iter()
                .filter_map(|(k, v)| {
                    let p = v.as_table()?.get("path")?.as_str()?;
                    Some((k.clone(), normalize(p)?))
                })
                .collect()
        })
        .unwrap_or_default();
    let mut members = Vec::new();
    for dir in dirs {
        let manifest = if dir.is_empty() {
            root.clone()
        } else {
            read_toml(&repo_root.join(&dir).join("Cargo.toml"))?
        };
        let name = manifest.get("package")?.as_table()?.get("name")?.as_str()?;
        let mut deps = BTreeSet::new();
        for table in dependency_tables(&manifest) {
            for (key, spec) in table {
                let Some(spec) = spec.as_table() else {
                    continue;
                };
                if let Some(p) = spec.get("path").and_then(toml::Value::as_str) {
                    deps.insert(normalize(&join(&dir, p))?);
                } else if spec.get("workspace").and_then(toml::Value::as_bool) == Some(true)
                    && let Some(p) = ws_deps.get(key)
                {
                    deps.insert(p.clone());
                }
            }
        }
        members.push(Member {
            name: name.to_string(),
            dir,
            deps,
        });
    }
    Some(members)
}

/// `[dependencies]`, `[dev-dependencies]`, `[build-dependencies]` and their
/// `[target.'cfg(..)'.*]` forms.
fn dependency_tables(manifest: &toml::Table) -> Vec<&toml::Table> {
    const KINDS: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];
    let mut out: Vec<&toml::Table> = KINDS
        .iter()
        .filter_map(|k| manifest.get(*k)?.as_table())
        .collect();
    if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
        for t in targets.values().filter_map(toml::Value::as_table) {
            out.extend(KINDS.iter().filter_map(|k| t.get(*k)?.as_table()));
        }
    }
    out
}

fn join(dir: &str, rel: &str) -> String {
    if dir.is_empty() {
        rel.to_string()
    } else {
        format!("{dir}/{rel}")
    }
}

/// `a/./b/../c/` -> `a/c`; `None` for an absolute path or one leaving the root.
fn normalize(path: &str) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for c in PathBuf::from(path).components() {
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
mod tests {
    use super::*;
    use aaf_domain::gate::ChangeKind;
    use tempfile::TempDir;

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    /// domain <- engine <- cli; fp stands alone; engine uses domain through
    /// `workspace = true`.
    fn workspace() -> TempDir {
        let t = TempDir::new().unwrap();
        let r = t.path();
        write(
            r,
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\", \"tools/fp\"]\n\
             [workspace.dependencies]\ndomain = { path = \"crates/domain\" }\n",
        );
        write(
            r,
            "crates/domain/Cargo.toml",
            "[package]\nname = \"domain\"\n",
        );
        write(
            r,
            "crates/engine/Cargo.toml",
            "[package]\nname = \"engine\"\n[dependencies]\ndomain = { workspace = true }\n",
        );
        write(
            r,
            "crates/cli/Cargo.toml",
            "[package]\nname = \"cli\"\n[dev-dependencies]\nengine = { path = \"../engine\" }\n",
        );
        write(r, "tools/fp/Cargo.toml", "[package]\nname = \"fp\"\n");
        t
    }

    fn modified(paths: &[&str]) -> Vec<Change> {
        paths
            .iter()
            .map(|p| Change {
                kind: ChangeKind::Modified,
                path: (*p).to_string(),
                old_path: None,
            })
            .collect()
    }

    fn names(set: Option<BTreeSet<String>>) -> Option<Vec<String>> {
        set.map(|s| s.into_iter().collect())
    }

    #[test]
    fn a_leaf_crate_selects_only_itself() {
        let t = workspace();
        let got = affected_packages(t.path(), &modified(&["crates/cli/src/main.rs"]));
        assert_eq!(names(got), Some(vec!["cli".to_string()]));
        let got = affected_packages(t.path(), &modified(&["tools/fp/src/lib.rs"]));
        assert_eq!(names(got), Some(vec!["fp".to_string()]));
    }

    #[test]
    fn a_base_crate_pulls_in_its_reverse_dependencies() {
        let t = workspace();
        let got = affected_packages(t.path(), &modified(&["crates/domain/src/lib.rs"]));
        assert_eq!(
            names(got),
            Some(vec![
                "cli".to_string(),
                "domain".to_string(),
                "engine".to_string()
            ])
        );
    }

    #[test]
    fn manifests_lockfile_build_scripts_and_outside_paths_widen() {
        let t = workspace();
        for p in [
            "Cargo.lock",
            "crates/cli/Cargo.toml",
            "crates/engine/build.rs",
            ".cargo/config.toml",
            "rust-toolchain.toml",
            "templates/prompt.md",
        ] {
            assert_eq!(affected_packages(t.path(), &modified(&[p])), None, "{p}");
        }
    }

    #[test]
    fn a_rename_counts_both_sides() {
        let t = workspace();
        let changes = vec![Change {
            kind: ChangeKind::Renamed,
            path: "crates/cli/src/x.rs".to_string(),
            old_path: Some("tools/fp/src/x.rs".to_string()),
        }];
        assert_eq!(
            names(affected_packages(t.path(), &changes)),
            Some(vec!["cli".to_string(), "fp".to_string()])
        );
    }

    #[test]
    fn expand_narrows_scoped_only_and_never_the_full_commands() {
        let t = workspace();
        let cmds = CommandsConfig {
            test_scoped: "cargo test {affected_packages}".to_string(),
            test_full: "cargo test {affected_packages}".to_string(),
            lint: "cargo clippy {affected_packages} --all-targets".to_string(),
            ..CommandsConfig::default()
        };
        let ch = modified(&["crates/engine/src/lib.rs"]);
        let (scoped, note) = expand(&cmds, GateMode::Scoped, t.path(), &ch).unwrap();
        assert_eq!(scoped.test_scoped, "cargo test -p cli -p engine");
        assert_eq!(scoped.lint, "cargo clippy -p cli -p engine --all-targets");
        assert_eq!(scoped.test_full, "cargo test --workspace");
        assert!(note.contains("2 of the workspace"), "{note}");
        let (full, _) = expand(&cmds, GateMode::Full, t.path(), &ch).unwrap();
        assert_eq!(full.test_scoped, "cargo test --workspace");
    }

    #[test]
    fn expand_falls_back_to_the_workspace_and_ignores_commands_without_it() {
        let t = workspace();
        let cmds = CommandsConfig {
            test_scoped: "cargo test {affected_packages}".to_string(),
            ..CommandsConfig::default()
        };
        let (out, _) = expand(&cmds, GateMode::Scoped, t.path(), &[]).unwrap();
        assert_eq!(out.test_scoped, "cargo test --workspace");
        let (out, _) =
            expand(&cmds, GateMode::Scoped, t.path(), &modified(&["README.md"])).unwrap();
        assert_eq!(out.test_scoped, "cargo test --workspace");
        let plain = CommandsConfig {
            test_scoped: "cargo test".to_string(),
            ..CommandsConfig::default()
        };
        assert!(expand(&plain, GateMode::Scoped, t.path(), &[]).is_none());
    }
}
