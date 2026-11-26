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
fn an_excluded_directory_is_no_member() {
    let t = workspace();
    write(
        t.path(),
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/*\", \"tools/fp\"]\nexclude = [\"crates/bench\"]\n\
         [workspace.dependencies]\ndomain = { path = \"crates/domain\" }\n",
    );
    write(
        t.path(),
        "crates/bench/Cargo.toml",
        "[package]\nname = \"bench\"\n[dependencies]\ndomain = { path = \"../domain\" }\n",
    );
    let got = affected_packages(t.path(), &modified(&["crates/domain/src/lib.rs"]));
    let got = names(got).unwrap();
    assert!(!got.contains(&"bench".to_string()), "{got:?}");
}

#[test]
fn the_full_commands_are_expanded_too() {
    let t = workspace();
    let commands = CommandsConfig {
        test_full: format!("cargo test {AFFECTED_PACKAGES}"),
        ..CommandsConfig::default()
    };
    let (out, _) = expand(&commands, GateMode::Full, t.path(), &[]).unwrap();
    assert_eq!(out.test_full, "cargo test --workspace");
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
    let (out, _) = expand(&cmds, GateMode::Scoped, t.path(), &modified(&["README.md"])).unwrap();
    assert_eq!(out.test_scoped, "cargo test --workspace");
    let plain = CommandsConfig {
        test_scoped: "cargo test".to_string(),
        ..CommandsConfig::default()
    };
    assert!(expand(&plain, GateMode::Scoped, t.path(), &[]).is_none());
}

/// Root `[package]` (`app`) + listed members `xtask` and `crates/a`, plus
/// unlisted non-path-dep `standalone` and unlisted `crates/a/sub`.
fn root_package_workspace() -> TempDir {
    let t = TempDir::new().unwrap();
    let r = t.path();
    write(
        r,
        "Cargo.toml",
        "[package]\nname = \"app\"\n[workspace]\nmembers = [\"xtask\", \"crates/a\"]\n",
    );
    write(r, "src/lib.rs", "");
    write(r, "tests/it.rs", "");
    write(r, "benches/bench.rs", "");
    write(r, "examples/ex.rs", "");
    write(r, "docs/x.md", "");
    write(r, "fixtures/golden.json", "");
    write(r, "README.md", "");
    write(r, "xtask/Cargo.toml", "[package]\nname = \"xtask\"\n");
    write(r, "crates/a/Cargo.toml", "[package]\nname = \"a\"\n");
    write(r, "crates/a/sub/Cargo.toml", "[package]\nname = \"sub\"\n");
    write(r, "standalone/Cargo.toml", "[package]\nname = \"solo\"\n");
    t
}

#[test]
fn root_package_narrows_listed_members_and_own_source_trees() {
    let t = root_package_workspace();
    for (path, want) in [
        ("xtask/src/lib.rs", "xtask"),
        ("crates/a/src/lib.rs", "a"),
        ("src/lib.rs", "app"),
        ("tests/it.rs", "app"),
        ("benches/bench.rs", "app"),
        ("examples/ex.rs", "app"),
    ] {
        let got = names(affected_packages(t.path(), &modified(&[path])));
        assert_eq!(got, Some(vec![want.to_string()]), "{path}");
    }
}

#[test]
fn root_package_widens_on_non_source_root_files_and_build_script() {
    let t = root_package_workspace();
    // Non-source paths at the repo root can be shared fixtures, docs, or
    // submodules without a top-level Cargo.toml; `build.rs` always widens.
    for p in [
        "docs/x.md",
        "fixtures/golden.json",
        "README.md",
        "vendor/sub",
        "build.rs",
    ] {
        assert_eq!(affected_packages(t.path(), &modified(&[p])), None, "{p}");
    }
}

#[test]
fn root_package_widens_on_unlisted_nested_crate() {
    let t = root_package_workspace();
    for p in [
        "standalone/src/lib.rs",
        "standalone",
        "../outside/lib.rs",
        "/abs/path.rs",
    ] {
        assert_eq!(affected_packages(t.path(), &modified(&[p])), None, "{p}");
    }
    let renamed = vec![Change {
        kind: ChangeKind::Renamed,
        path: "src/lib.rs".to_string(),
        old_path: Some("standalone/src/old.rs".to_string()),
    }];
    assert_eq!(affected_packages(t.path(), &renamed), None);
}

#[test]
fn listed_member_widens_on_unlisted_nested_crate() {
    let t = root_package_workspace();
    for p in ["crates/a/sub/src/lib.rs", "crates/a/sub"] {
        assert_eq!(affected_packages(t.path(), &modified(&[p])), None, "{p}");
    }
    // Listing the nested crate lets longest-prefix own it again.
    write(
        t.path(),
        "Cargo.toml",
        "[package]\nname = \"app\"\n\
         [workspace]\nmembers = [\"xtask\", \"crates/a\", \"crates/a/sub\"]\n",
    );
    let got = names(affected_packages(
        t.path(),
        &modified(&["crates/a/sub/src/lib.rs"]),
    ));
    assert_eq!(got, Some(vec!["sub".to_string()]));
}

#[test]
fn virtual_workspace_widens_on_transitive_unlisted_path_dep() {
    let t = TempDir::new().unwrap();
    let r = t.path();
    write(
        r,
        "Cargo.toml",
        "[workspace]\nmembers = [\"app\", \"base\"]\n",
    );
    write(
        r,
        "app/Cargo.toml",
        "[package]\nname = \"app\"\n[dependencies]\nmid = { path = \"../mid\" }\n",
    );
    write(
        r,
        "mid/Cargo.toml",
        "[package]\nname = \"mid\"\n[dependencies]\nbase = { path = \"../base\" }\n",
    );
    write(r, "base/Cargo.toml", "[package]\nname = \"base\"\n");
    assert_eq!(affected_packages(r, &modified(&["base/src/lib.rs"])), None);
}

#[test]
fn unlisted_or_unnormalizable_path_dep_in_any_dep_table_widens() {
    // [target.*.dev-dependencies] pointing to an unlisted crate
    let t = workspace();
    write(
        t.path(),
        "crates/cli/Cargo.toml",
        "[package]\nname = \"cli\"\n\
         [target.'cfg(unix)'.dev-dependencies]\n\
         extra = { path = \"../extra\" }\n",
    );
    assert_eq!(
        affected_packages(t.path(), &modified(&["crates/domain/src/lib.rs"])),
        None
    );

    // [workspace.dependencies] pointing to an unlisted crate
    let t = workspace();
    write(
        t.path(),
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/*\", \"tools/fp\"]\n\
         [workspace.dependencies]\n\
         domain = { path = \"crates/domain\" }\n\
         extra = { path = \"extra\" }\n",
    );
    assert_eq!(
        affected_packages(t.path(), &modified(&["crates/domain/src/lib.rs"])),
        None
    );

    // Unnormalizable path dep leaving root or absolute
    for bad_path in ["../../../outside", "/abs/crate"] {
        let t = workspace();
        write(
            t.path(),
            "crates/cli/Cargo.toml",
            &format!(
                "[package]\nname = \"cli\"\n[build-dependencies]\next = {{ path = \"{bad_path}\" }}\n"
            ),
        );
        assert_eq!(
            affected_packages(t.path(), &modified(&["crates/domain/src/lib.rs"])),
            None,
            "{bad_path}"
        );
    }
}

#[test]
fn root_patch_or_replace_with_path_widens() {
    let t = TempDir::new().unwrap();
    let r = t.path();
    write(
        r,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/a\", \"vendor/foo\"]\n\
         [patch.crates-io]\nfoo = { path = \"vendor/foo\" }\n",
    );
    write(
        r,
        "crates/a/Cargo.toml",
        "[package]\nname = \"a\"\n[dependencies]\nfoo = \"1\"\n",
    );
    write(r, "vendor/foo/Cargo.toml", "[package]\nname = \"foo\"\n");
    assert_eq!(
        affected_packages(r, &modified(&["vendor/foo/src/x.rs"])),
        None
    );

    // Custom registry URL with path also widens
    write(
        r,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/a\", \"vendor/foo\"]\n\
         [patch.\"https://github.com/example/repo\"]\nfoo = { path = \"vendor/foo\" }\n",
    );
    assert_eq!(
        affected_packages(r, &modified(&["vendor/foo/src/x.rs"])),
        None
    );

    // [replace] with path also widens
    write(
        r,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/a\", \"vendor/foo\"]\n\
         [replace]\n\"foo:1.0.0\" = { path = \"vendor/foo\" }\n",
    );
    assert_eq!(
        affected_packages(r, &modified(&["vendor/foo/src/x.rs"])),
        None
    );

    // Non-path [patch] (git) does not widen
    write(
        r,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/a\", \"vendor/foo\"]\n\
         [patch.crates-io]\nfoo = { git = \"https://github.com/example/foo\" }\n",
    );
    assert_eq!(
        names(affected_packages(r, &modified(&["vendor/foo/src/x.rs"]))),
        Some(vec!["foo".to_string()])
    );
}

/// `crates/a` depends on registry `foo = "1"`; `vendor/foo` is a listed
/// member that a `.cargo/config` may swap in for it.
fn config_patch_workspace() -> TempDir {
    let t = TempDir::new().unwrap();
    let r = t.path();
    write(
        r,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/a\", \"vendor/foo\"]\n",
    );
    write(
        r,
        "crates/a/Cargo.toml",
        "[package]\nname = \"a\"\n[dependencies]\nfoo = \"1\"\n",
    );
    write(r, "vendor/foo/Cargo.toml", "[package]\nname = \"foo\"\n");
    t
}

#[test]
fn cargo_config_patch_or_paths_widens() {
    let foo = modified(&["vendor/foo/src/x.rs"]);
    for (file, body) in [
        (
            ".cargo/config.toml",
            "[patch.crates-io]\nfoo = { path = \"vendor/foo\" }\n",
        ),
        (".cargo/config.toml", "paths = [\"vendor/foo\"]\n"),
        (
            ".cargo/config",
            "[patch.crates-io]\nfoo = { path = \"vendor/foo\" }\n",
        ),
        (".cargo/config.toml", "[patch.crates-io\nfoo = \n"),
    ] {
        let t = config_patch_workspace();
        write(t.path(), file, body);
        assert_eq!(affected_packages(t.path(), &foo), None, "{file}: {body}");
    }
}

#[test]
fn cargo_config_without_patch_or_paths_keeps_narrowing() {
    let foo = modified(&["vendor/foo/src/x.rs"]);
    let t = config_patch_workspace();
    let without = names(affected_packages(t.path(), &foo));
    assert_eq!(without, Some(vec!["foo".to_string()]));
    write(t.path(), ".cargo/config.toml", "[build]\njobs = 4\n");
    assert_eq!(names(affected_packages(t.path(), &foo)), without);
}

#[test]
fn underscore_dependency_tables_count_as_reverse_dependencies() {
    let engine = modified(&["crates/engine/src/lib.rs"]);
    let want = Some(vec!["cli".to_string(), "engine".to_string()]);
    for table in [
        "[dev_dependencies]",
        "[target.'cfg(unix)'.build_dependencies]",
    ] {
        let t = workspace();
        write(
            t.path(),
            "crates/cli/Cargo.toml",
            &format!("[package]\nname = \"cli\"\n{table}\nengine = {{ path = \"../engine\" }}\n"),
        );
        assert_eq!(names(affected_packages(t.path(), &engine)), want, "{table}");
        // An unlisted path target in the same table widens.
        write(
            t.path(),
            "crates/cli/Cargo.toml",
            &format!("[package]\nname = \"cli\"\n{table}\nextra = {{ path = \"../../extra\" }}\n"),
        );
        assert_eq!(affected_packages(t.path(), &engine), None, "{table}");
    }
}
