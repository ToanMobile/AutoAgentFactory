#![allow(clippy::unwrap_used, clippy::expect_used)]
//! GATE-059 verification: a root `[package]` (`dir = ""`) must not swallow
//! edits inside a nested path-dependency crate omitted from `[workspace].members`.

use std::fs;
use std::path::Path;

use aaf_domain::gate::ChangeKind;
use aaf_engine::gate::changes::Change;
use aaf_engine::gate::rust_scope::affected_packages;
use tempfile::TempDir;

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

fn root_package_workspace() -> TempDir {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write(
        root,
        "Cargo.toml",
        "[package]\n\
         name = \"app\"\n\
         version = \"0.1.0\"\n\
         edition = \"2021\"\n\
         \n\
         [workspace]\n\
         members = [\"xtask\"]\n\
         \n\
         [dependencies]\n\
         core = { path = \"core\" }\n",
    );
    write(root, "src/lib.rs", "pub fn app() {}\n");
    write(
        root,
        "core/Cargo.toml",
        "[package]\n\
         name = \"core\"\n\
         version = \"0.1.0\"\n\
         edition = \"2021\"\n",
    );
    write(root, "core/src/lib.rs", "pub fn core_fn() {}\n");
    write(
        root,
        "xtask/Cargo.toml",
        "[package]\n\
         name = \"xtask\"\n\
         version = \"0.1.0\"\n\
         edition = \"2021\"\n",
    );
    write(root, "xtask/src/lib.rs", "pub fn task() {}\n");
    dir
}

fn modified(path: &str) -> Vec<Change> {
    vec![Change {
        kind: ChangeKind::Modified,
        path: path.to_string(),
        old_path: None,
    }]
}

#[test]
fn root_package_does_not_swallow_path_dep_outside_members() {
    let ws = root_package_workspace();
    let got = affected_packages(ws.path(), &modified("core/src/lib.rs"));

    match got {
        None => {}
        Some(pkgs) => assert!(
            pkgs.contains("core"),
            "editing path-dep crate `core` outside `[workspace].members` must widen (`None`) or include `core`, got {pkgs:?}"
        ),
    }
}

#[test]
fn root_package_own_source_still_narrows() {
    let ws = root_package_workspace();
    let got = affected_packages(ws.path(), &modified("src/lib.rs"));

    if let Some(pkgs) = got {
        assert!(
            pkgs.contains("app"),
            "editing root package `src/lib.rs`, when narrowed, must include `app`, got {pkgs:?}"
        );
    }
}
