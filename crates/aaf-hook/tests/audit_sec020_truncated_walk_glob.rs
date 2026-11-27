#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Regression tests for SEC-020: a glob in a command argument is expanded by
//! walking the tree, capped by `WALK_ENTRIES` (2000) and `WALK_DEPTH` (8) in
//! `delete_guard.rs` and `secret_guard.rs`. When the walk is cut, both guards
//! fall back to a static list of sample names (`SAMPLE_NAMES` /
//! `SECRET_SAMPLES`). A last component such as `release.keystor?` names
//! `release.keystore` but matches no sample (`a.keystore`), so a cut walk lets
//! `rm` / `cat` of a real keystore through. A cut walk must fail closed for any
//! last component that can name a protected file; ordinary names stay allowed.

mod common;

use std::fs;
use std::path::Path;

use common::{Fixture, run_corpus};

const ALLOW: Option<&str> = None;
const DELETE: Option<&str> = Some("delete_guard");
const SECRET: Option<&str> = Some("secret_guard");

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

/// The sibling directory the root listing yields last, after the walk's
/// entry cap: the walk is cut before it ever lists it, whatever order the
/// file system lists entries in.
fn last_listed_sibling(root: &Path) -> String {
    let names: Vec<String> = fs::read_dir(root)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    let position = names
        .iter()
        .rposition(|name| name.starts_with('d'))
        .unwrap();
    assert!(
        position >= 2000,
        "last sibling listed at {position}, before the walk cap"
    );
    names[position].clone()
}

/// A tree nine directories deep, one level past `WALK_DEPTH`, with a real
/// keystore and an ordinary file at the bottom.
fn seed_deep(root: &Path) -> &'static str {
    let deep = "a/b/c/e/f/g/h/i/j";
    let dir = root.join(deep);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("release.keystore"), "secret\n").unwrap();
    fs::write(dir.join("f.txt"), "plain\n").unwrap();
    deep
}

#[test]
fn glob_naming_keystore_is_refused_when_walk_is_cut_by_entry_cap() {
    let fx = Fixture::new();
    seed_siblings(&fx.root);
    let hidden = last_listed_sibling(&fx.root);
    fs::write(fx.root.join(&hidden).join("release.keystore"), "secret\n").unwrap();
    assert!(fx.root.join(&hidden).join("release.keystore").is_file());
    run_corpus(
        &fx,
        &[
            ("rm */release.keystor?", DELETE),
            ("rm -f */release.keystor?", DELETE),
            ("rm d*/release.keystor?", DELETE),
            ("cat */release.keystor?", SECRET),
            ("cat d*/release.keystor?", SECRET),
        ],
    );
}

#[test]
fn glob_naming_keystore_is_refused_when_walk_is_cut_by_depth_cap() {
    let fx = Fixture::new();
    let deep = seed_deep(&fx.root);
    assert!(fx.root.join(deep).join("release.keystore").is_file());
    let glob = format!("{}release.keystor?", "*/".repeat(9));
    let rm = format!("rm {glob}");
    let rm_f = format!("rm -f {glob}");
    let cat = format!("cat {glob}");
    run_corpus(
        &fx,
        &[
            (rm.as_str(), DELETE),
            (rm_f.as_str(), DELETE),
            (cat.as_str(), SECRET),
        ],
    );
}

#[test]
fn ordinary_globs_stay_allowed_when_walk_is_cut() {
    let fx = Fixture::new();
    seed_siblings(&fx.root);
    seed_deep(&fx.root);
    let deep_glob = format!("{}f.tx?", "*/".repeat(9));
    let deep_rm = format!("rm {deep_glob}");
    let deep_cat = format!("cat {deep_glob}");
    run_corpus(
        &fx,
        &[
            ("rm */f.tx?", ALLOW),
            ("rm -f */f.tx?", ALLOW),
            ("cat d000*/f.txt", ALLOW),
            ("cat */f.tx?", ALLOW),
            (deep_rm.as_str(), ALLOW),
            (deep_cat.as_str(), ALLOW),
        ],
    );
}
