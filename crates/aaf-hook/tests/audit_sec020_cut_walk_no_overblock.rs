#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for SEC-020 rework 1: a cut glob walk must not overblock
//! open-ended globs (ending with `*`) or literal filenames whose parent is not
//! a guarded directory, while copiers with a globbed parent and a bounded
//! protected-name glob (`rsync`/`install d*/release.keystor? ./`) must still be
//! denied by `delete_guard`.

mod common;

use std::fs;
use std::path::Path;

use common::{Fixture, run_corpus};

const ALLOW: Option<&str> = None;
const DELETE: Option<&str> = Some("delete_guard");

/// More sibling directories than `WALK_ENTRIES`, so listing the repository
/// root alone cuts the walk.
const SIBLINGS: usize = 2100;

/// Seeds `SIBLINGS` directories `dNNNN` under the repository root, each with
/// an ordinary `f.txt`.
fn seed_siblings(root: &Path) {
    for i in 0..SIBLINGS {
        let dir = root.join(format!("d{i:04}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("f.txt"), "plain\n").unwrap();
    }
}

#[test]
fn cut_walk_allows_open_globs_and_non_guarded_literals() {
    let fx = Fixture::new();
    seed_siblings(&fx.root);
    run_corpus(
        &fx,
        &[
            ("cat **/README*", ALLOW),
            ("grep foo **/mod.*", ALLOW),
            ("rm -f **/core.*", ALLOW),
            ("rm -f **/dist/settings.json", ALLOW),
            ("rm **/build/config.toml", ALLOW),
            ("rm **/build/*.toml", ALLOW),
            ("rm **/dist/settings.jso?", ALLOW),
            ("cat */f.tx?", ALLOW),
            ("rm **/.claude/settings.jso?", DELETE),
            ("rm **/*.toml", DELETE),
            ("rm **/settings.jso?", DELETE),
        ],
    );
}

#[test]
fn copier_with_glob_parent_allows_toml_into_non_guarded_dir() {
    let fx = Fixture::new();
    fx.write("crates/a/Cargo.toml", "[package]\nname = \"a\"\n");
    fx.write("crates/b/x.toml", "k = 1\n");
    fs::create_dir_all(fx.root.join("dst")).unwrap();
    fs::create_dir_all(fx.root.join("out")).unwrap();
    run_corpus(
        &fx,
        &[
            ("cp crates/*/*.toml dst/", ALLOW),
            ("rsync -a crates/*/*.toml out/", ALLOW),
        ],
    );
}

#[test]
fn copier_with_glob_parent_blocks_bounded_protected_glob() {
    let fx = Fixture::new();
    seed_siblings(&fx.root);
    let keystore = fx.root.join("d0001").join("release.keystore");
    fs::write(&keystore, "secret\n").unwrap();
    assert!(keystore.is_file());
    run_corpus(
        &fx,
        &[
            ("rsync d*/release.keystor? ./", DELETE),
            ("install d*/release.keystor? ./", DELETE),
        ],
    );
}
